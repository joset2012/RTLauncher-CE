use actix::prelude::*;
use actix_cors::Cors;
use actix_files as fs;
use actix_web::{
    get, post,
    web::{Data, Json, Path, Payload},
    App, HttpRequest, HttpResponse, HttpServer, Responder,
};
use actix_web_actors::ws;
use futures::{
    executor::block_on,
    future::{AbortHandle, Abortable, Aborted},
};
use once_cell::sync::Lazy;
use serde_json::json;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
};
use tokio::{sync::Mutex, task};
use uuid::Uuid;
use futures::future::{BoxFuture, FutureExt};

mod tasks;
use tasks::*;

// 状态表&共享类型

#[derive(Clone, Serialize)]
#[serde(rename_all = "lowercase")]
enum TaskState {
    Queued,
    Running,
    Completed,
    Failed,
    Canceled,
}

static TASK_STATUS: Lazy<Mutex<HashMap<Uuid, (TaskState, String)>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

type Cancelled = Arc<Mutex<HashSet<Uuid>>>;
type Running   = Arc<Mutex<HashMap<Uuid, AbortHandle>>>;

type Sessions = Arc<Mutex<HashMap<Uuid, Addr<WebSocketSession>>>>;

// websocket

#[derive(Message)]
#[rtype(result = "()")]
struct WsMessage(pub String);

struct WebSocketSession {
    id: Uuid,
    sessions: Sessions,
}

impl Actor for WebSocketSession {
    type Context = ws::WebsocketContext<Self>;
    fn started(&mut self, ctx: &mut Self::Context) {
        let addr = ctx.address();
        block_on(self.sessions.lock()).insert(self.id, addr);
    }
    fn stopped(&mut self, _ctx: &mut Self::Context) {
        block_on(self.sessions.lock()).remove(&self.id);
    }
}
impl Handler<WsMessage> for WebSocketSession {
    type Result = ();
    fn handle(&mut self, msg: WsMessage, ctx: &mut Self::Context) {
        ctx.text(msg.0);
    }
}
impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for WebSocketSession {
    fn handle(&mut self, _: Result<ws::Message, ws::ProtocolError>, _: &mut Self::Context) {}
}

// 任务列表

#[derive(Clone)]
struct Job {
    id: Uuid,
    module: String,
    version: Option<String>,
}
#[derive(Default)]
struct QueueState {
    running: bool,
    queue: VecDeque<Job>,
}
type Queue = Arc<Mutex<QueueState>>;

// DTO

#[derive(Deserialize)]
struct SubmitRequest {
    module: String,
    version: Option<String>,
}
#[derive(Serialize)]
struct SubmitResp {
    id: Uuid,
}
#[derive(Deserialize)]
struct FeedbackReq {
    id: Uuid,
    ack: String,
}
#[derive(Deserialize)]
struct InterruptReq {
    id: Uuid,
}

// 路由

#[post("/submit")]
async fn submit(
    queue:     Data<Queue>,
    sessions:  Data<Sessions>,
    running:   Data<Running>,
    cancelled: Data<Cancelled>,
    Json(req): Json<SubmitRequest>,
) -> impl Responder {
    let id = Uuid::new_v4();
    TASK_STATUS.lock().await.insert(id, (TaskState::Queued, "".into()));

    queue.lock().await.queue.push_back(Job{
        id, module:req.module, version:req.version,
    });

    // 如果当前没有 worker，再启动一个
    if !queue.lock().await.running {
        queue.lock().await.running = true;
        spawn_worker(
            queue.clone(),
            sessions.clone(),
            running.clone(),
            cancelled.clone(),
        );
    }

    HttpResponse::Ok().json(SubmitResp { id })
}

#[get("/ws/{id}")]
async fn ws_index(
    req: HttpRequest,
    stream: Payload,
    path: Path<Uuid>,
    sessions: Data<Sessions>,
) -> actix_web::Result<HttpResponse> {
    ws::start(
        WebSocketSession { id: path.into_inner(), sessions: sessions.get_ref().clone() },
        &req,
        stream,
    )
}

#[post("/feedback")]
async fn feedback(Json(req): Json<FeedbackReq>) -> impl Responder {
    println!("ACK {}: {}", req.id, req.ack);
    HttpResponse::Ok().body("received")
}

#[post("/interrupt")]
async fn interrupt(
    Json(req): Json<InterruptReq>,
    running: Data<Running>,
    cancelled: Data<Cancelled>,
) -> impl Responder {
    // 执行中
    if let Some(h) = running.lock().await.remove(&req.id) {
        h.abort();
        TASK_STATUS.lock().await.insert(req.id, (TaskState::Canceled, "killed running".into()));
        return HttpResponse::Ok().body("running task aborted");
    }
    // 排队中
    cancelled.lock().await.insert(req.id);
    TASK_STATUS.lock().await.insert(req.id, (TaskState::Canceled, "canceled before run".into()));
    HttpResponse::Ok().body("queued task canceled")
}

#[get("/tasks")]
async fn list_tasks() -> impl Responder {
    let map = TASK_STATUS.lock().await;
    let list: Vec<_> = map.iter()
        .map(|(id, (state, info))| json!({ "id": id, "state": state, "info": info }))
        .collect();
    HttpResponse::Ok().json(list)
}

// worker


fn spawn_worker(
    queue: Data<Queue>,
    sessions: Data<Sessions>,
    running: Data<Running>,
    cancelled: Data<Cancelled>,
) {
    task::spawn(async move {
        loop {
            // 取队首任务
            let job = { queue.lock().await.queue.pop_front() };
            let job = match job {
                Some(j) => j,
                None => {
                    queue.lock().await.running = false;
                    break;
                }
            };

            // 如果已取消则跳过
            if cancelled.lock().await.remove(&job.id) {
                TASK_STATUS.lock().await.insert(
                    job.id,
                    (TaskState::Canceled, "canceled in queue".into()),
                );
                continue;
            }

            TASK_STATUS.lock().await.insert(job.id, (TaskState::Running, "".into()));

            // 拆解 job 获取所有权
            let Job { id: task_id, module, version } = job;

            // 构造任务 Future
            let fut: BoxFuture<'static, anyhow::Result<String>> = match module.as_str() {
                "classify_versions" => {
                    async { classify_versions_task().await }.boxed()
                }
                "original_download" => {
                    let mc_home = std::path::Path::new(
                        r"C:\Users\smh20\Documents\Rust\RTAPI\.minecraft",
                    );
                    let ver = version.as_deref().unwrap_or("1.20.4").to_owned();
                    async move { original_download_task(&ver, mc_home).await }.boxed()
                }
                other => {
                    let msg = format!("Unknown module: {other}");
                    async move { Ok(msg) }.boxed()
                }
            };

            // 包装为可中断任务
            let (handle, reg) = AbortHandle::new_pair();
            let abortable = Abortable::new(fut, reg);
            running.lock().await.insert(task_id, handle);

            // 执行任务
            let text = match abortable.await {
                Ok(Ok(s)) => {
                    TASK_STATUS
                        .lock()
                        .await
                        .insert(task_id, (TaskState::Completed, s.clone()));
                    s
                }
                Ok(Err(e)) => {
                    TASK_STATUS
                        .lock()
                        .await
                        .insert(task_id, (TaskState::Failed, e.to_string()));
                    format!("Task failed: {e}")
                }
                Err(Aborted) => {
                    TASK_STATUS
                        .lock()
                        .await
                        .insert(task_id, (TaskState::Canceled, "by interrupt".into()));
                    "Task canceled".into()
                }
            };

            running.lock().await.remove(&task_id);

            // 推送消息
            if let Some(addr) = sessions.lock().await.get(&task_id) {
                addr.do_send(WsMessage(text));
            }
        }
    });
}

// main

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
    let queue: Queue = Arc::new(Mutex::new(QueueState::default()));
    let running: Running = Arc::new(Mutex::new(HashMap::new()));
    let cancelled: Cancelled = Arc::new(Mutex::new(HashSet::new()));

    // worker启动
    spawn_worker(
        Data::new(queue.clone()),
        Data::new(sessions.clone()),
        Data::new(running.clone()),
        Data::new(cancelled.clone()),
    );

    HttpServer::new(move || {
        App::new()
            .wrap(
                Cors::default()
                    .allow_any_origin()
                    .allow_any_header()
                    .allow_any_method()
                    .max_age(3600),
            )
            .app_data(Data::new(sessions.clone()))
            .app_data(Data::new(queue.clone()))
            .app_data(Data::new(running.clone()))
            .app_data(Data::new(cancelled.clone()))
            .service(submit)
            .service(ws_index)
            .service(interrupt)
            .service(list_tasks)
            .service(feedback)
            .service(fs::Files::new("/", "./").index_file("index.html"))
    })
        .bind(("0.0.0.0", 3000))?
        .run()
        .await
}
