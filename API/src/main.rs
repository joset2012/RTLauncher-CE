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
use futures::future::{FutureExt};
use std::path::PathBuf;
mod tasks;
use tasks::*;
use std::pin::Pin;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};


static WORKERS: AtomicUsize = AtomicUsize::new(0);

// 状态表和共享类型

#[derive(Clone, Serialize)]
#[serde(rename_all = "lowercase")]
enum TaskState {
    Queued,
    Running,
    Completed,
    Failed,
    Canceled,
}

type BoxedTask = Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send>>;

// 状态表
static TASK_STATUS: Lazy<
    Mutex<HashMap<Uuid, (TaskState, String /*module*/, String /*info*/ )>>,
> = Lazy::new(|| Mutex::new(HashMap::new()));

// 运行中句柄
type Running = Arc<Mutex<HashMap<Uuid, AbortHandle>>>;
type Sessions = Arc<Mutex<HashMap<Uuid, Addr<WebSocketSession>>>>;
type Cancelled = Arc<Mutex<HashSet<Uuid>>>;

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
    id:      Uuid,
    module:  String,
    version: Option<String>,

    minecraft_path:   Option<String>,
    java_path:        Option<String>,
    wrapper_path:     Option<String>,
    launcher_version: Option<String>,
    max_memory:       Option<String>,
    version_type:     Option<String>,
    player: Option<String>,
    token:  Option<String>,
    uuid:   Option<String>,
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
#[derive(Deserialize)]
struct SubmitPayload {
    module: String,
    version: Option<String>,

    minecraft_path:   Option<String>,
    java_path:        Option<String>,
    wrapper_path:     Option<String>,
    launcher_version: Option<String>,
    max_memory:       Option<String>,
    version_type:     Option<String>,
    player: Option<String>,
    token:  Option<String>,
    uuid:   Option<String>,
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
    running:   Data<Running>,
    cancelled: Data<Cancelled>,
    sessions:  Data<Sessions>,
    Json(req): Json<SubmitPayload>,
) -> impl Responder {
    let id = Uuid::new_v4();

    // Queued
    TASK_STATUS
        .lock()
        .await
        .insert(id, (TaskState::Queued, req.module.clone(), "".into()));

    // 构造入队
    queue.lock().await.queue.push_back(Job {
        id,
        module:  req.module.clone(),
        version: req.version,
        minecraft_path:   req.minecraft_path,
        java_path:        req.java_path,
        wrapper_path:     req.wrapper_path,
        launcher_version: req.launcher_version,
        max_memory:       req.max_memory,
        version_type:     req.version_type,
        player: req.player,
        token:  req.token,
        uuid:   req.uuid,
    });

    // 若当前没有活跃worker则启动一个
    if WORKERS.fetch_add(1, Ordering::SeqCst) == 0 {
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
    // abort
    if let Some(ab) = running.lock().await.remove(&req.id) {
        ab.abort();
        TASK_STATUS.lock().await.insert(
            req.id,
            (TaskState::Canceled, "running".into(), "aborted".into()),
        );
        return HttpResponse::Ok().body("running task canceled");
    }

    // 标记取消
    cancelled.lock().await.insert(req.id);
    TASK_STATUS.lock().await.insert(
        req.id,
        (TaskState::Canceled, "queued".into(), "canceled before run".into()),
    );
    HttpResponse::Ok().body("queued task canceled")
}



#[get("/tasks")]
async fn list_tasks() -> impl Responder {
    let map = TASK_STATUS.lock().await;
    let list: Vec<_> = map
        .iter()
        .map(|(id, (state, module, info))| json!({
            "id": id,
            "module": module,
            "state": state,
            "info": info
        }))
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
            // 取队首，若为空则线程退出
            let job = match { queue.lock().await.queue.pop_front() } {
                Some(j) => j,
                None => break,
            };

            // 排队阶段被取消
            if cancelled.lock().await.remove(&job.id) {
                TASK_STATUS.lock().await.insert(
                    job.id,
                    (TaskState::Canceled, job.module.clone(), "canceled in queue".into()),
                );
                continue;
            }

            TASK_STATUS.lock().await.insert(
                job.id,
                (TaskState::Running, job.module.clone(), "".into()),
            );

            let task_id     = job.id;
            let module_name = job.module.clone();

            let fut = build_task(job);

            // abortable句柄登记
            let (ab, reg) = AbortHandle::new_pair();
            running.lock().await.insert(task_id, ab);
            let result = Abortable::new(fut, reg).await;

            // 结果状态
            let (state, info) = match result {
                Ok(Ok(s))   => (TaskState::Completed, s),
                Ok(Err(e))  => (TaskState::Failed, e.to_string()),
                Err(Aborted)=> (TaskState::Canceled, "by interrupt".into()),
            };
            TASK_STATUS
                .lock()
                .await
                .insert(task_id, (state, module_name.clone(), info));

            running.lock().await.remove(&task_id);

            // 推送日志
            if let Some(addr) = sessions.lock().await.get(&task_id) {
                addr.do_send(WsMessage(module_name + " done"));
            }
        }

        // 线程结束，计数-1
        WORKERS.fetch_sub(1, Ordering::SeqCst);
    });
}

/// 异步任务
fn build_task(job: Job) -> BoxedTask {
    match job.module.as_str() {
        "classify_versions" => Box::pin(async { classify_versions_task().await }),

        "original_download" => {
            let ver     = job.version.unwrap_or_else(|| "1.20.4".into());
            let mc_home = std::env::current_dir().unwrap().join(".minecraft");
            Box::pin(async move { original_download_task(&ver, &mc_home).await })
        }

        "launch_game" => {
            let cfg = tasks::launcher::LauncherConfig {
                minecraft_path: PathBuf::from(
                    job.minecraft_path.unwrap_or_else(|| "C:/Users/smh20/.minecraftx".into())),
                java_path: PathBuf::from(
                    job.java_path.unwrap_or_else(|| "C:/Program Files/Java/jdk-17/bin/java.exe".into())),
                wrapper_path: PathBuf::from(
                    job.wrapper_path.unwrap_or_else(|| "C:/Users/smh20/Documents/Rust/java_launch_wrapper-1.4.3.jar".into())),
                launcher_version: job.launcher_version.unwrap_or_else(|| "1.0.0".into()),
                max_memory: job.max_memory.unwrap_or_else(|| "16384".into()),
                version_type: job.version_type.unwrap_or_else(|| "NULL11034".into()),
            };
            let ver  = job.version.unwrap_or_else(|| "1.19.2".into());
            let name = job.player.unwrap_or_else(|| "Player".into());
            let tok  = job.token.unwrap_or_else(|| "mc_token".into());
            let uid  = job.uuid.unwrap_or_else(|| "uuid".into());

            Box::pin(async move {
                let args = tasks::launcher::build_jvm_arguments(&cfg, &ver, &name, &tok, &uid)?;
                let status = tokio::process::Command::new(&cfg.java_path)
                    .args(&args)
                    .status()
                    .await?;
                Ok::<_, anyhow::Error>(format!("game exit: {status}"))
            })
        }

        other => {
            let msg = format!("Unknown module: {other}");
            Box::pin(async move { Ok(msg) })
        }
    }
}

// main

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
    let queue: Queue = Arc::new(Mutex::new(QueueState::default()));
    let running: Running = Arc::new(Mutex::new(HashMap::new()));
    let cancelled: Cancelled = Arc::new(Mutex::new(HashSet::new()));

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
        .bind(("localhost", 3000))?
        .run()
        .await
}
