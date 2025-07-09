use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Semaphore;
use anyhow::{anyhow, Result};

const MOJANG_MANIFEST: &str = "https://piston-meta.mojang.com/mc/game/version_manifest.json";
const MIRROR_URL: &str = "https://bmclapi2.bangbang93.com";
const MAX_CONCURRENT_DOWNLOADS: usize = 64 ;

#[derive(Debug)]
struct DownloadTask {
    urls: Vec<String>,
    target_path: PathBuf,
    sha1: String,
    size: u64,  
}

struct DownloadProgress {
    total: Arc<AtomicUsize>,
    success: Arc<AtomicUsize>,
    failed: Arc<AtomicUsize>,
}

impl DownloadProgress {
    fn new(total: usize) -> Self {
        Self {
            total: Arc::new(AtomicUsize::new(total)),
            success: Arc::new(AtomicUsize::new(0)),
            failed: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct VersionManifest {
    versions: Vec<VersionEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
struct VersionEntry {
    id: String,
    url: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct VersionJson {
    downloads: Downloads,
    #[serde(default)]
    logging: Option<Logging>,
    #[serde(rename = "assetIndex")]
    asset_index: AssetIndex,
    libraries: Vec<Library>,
    #[serde(flatten)]
    other: serde_json::Value,
}

#[derive(Debug, Deserialize, Serialize)]
struct AssetIndex {
    url: String,
    id: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct AssetsJson {
    objects: HashMap<String, AssetObject>,
}

#[derive(Debug, Deserialize, Serialize)]
struct AssetObject {
    hash: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct Downloads {
    client: ClientDownload,
}

#[derive(Debug, Deserialize, Serialize)]
struct ClientDownload {
    url: String,
    sha1: String,
    size: u64,
}

#[derive(Debug, Deserialize, Serialize)]
struct Logging {
    client: LoggingClient,
}

#[derive(Debug, Deserialize, Serialize)]
struct LoggingClient {
    file: LogFile,
}

#[derive(Debug, Deserialize, Serialize)]
struct LogFile {
    url: String,
    sha1: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct Library {
    name: String,
    downloads: LibraryDownloads,
    #[serde(default)]
    rules: Vec<Rule>,
    #[serde(default)]
    natives: HashMap<String, String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct LibraryDownloads {
    artifact: Option<Artifact>,
    #[serde(default)]
    classifiers: HashMap<String, Artifact>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Artifact {
    path: String,
    sha1: String,
    size: u64,
}

#[derive(Debug, Deserialize, Serialize)]
struct Rule {
    action: String,
    #[serde(default)]
    os: Option<OsRule>,
}

#[derive(Debug, Deserialize, Serialize)]
struct OsRule {
    name: Option<String>,
}


async fn download_task(
    task: DownloadTask,
    semaphore: Arc<Semaphore>,
    progress: Option<Arc<DownloadProgress>>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let _permit = semaphore.acquire().await?;
    const MAX_RETRIES: u8 = 5;  // 增加重试次数
    let mut retry_count = 0;
    let mut used_urls: Vec<String> = Vec::new();

    loop {
        // 先检查已有文件的完整性
        if let Ok(mut file) = File::open(&task.target_path).await {
            match check_sha1(&mut file, &task.sha1).await {
                Ok(true) => {
                    if let Some(p) = &progress {
                        p.success.fetch_add(1, Ordering::SeqCst);
                    }
                    return Ok(());
                }
                Ok(false) => {
                    eprintln!("文件校验失败，触发重新下载: {}", task.target_path.display());
                    fs::remove_file(&task.target_path)?;
                }
                Err(e) => {
                    eprintln!("校验读取失败: {}", e);
                    fs::remove_file(&task.target_path)?;
                }
            }
        }

        if let Some(parent) = task.target_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let current_url = task.urls.iter()
            .find(|url| !used_urls.contains(url))
            .or_else(|| task.urls.first());
        let (result, url_used) = match current_url {
            Some(url) => {
                println!("Downloading from: {}", url);
                used_urls.push(url.to_string());
                (download_with_url(url, &task).await, url.to_string())
            }
            None => break,
        };

        match result {
            Ok(_) => {
                // 下载完成后再次校验
                let mut file = File::open(&task.target_path).await?;
                if check_sha1(&mut file, &task.sha1).await? {
                    if let Some(p) = &progress {
                        p.success.fetch_add(1, Ordering::SeqCst);
                    }
                    return Ok(());
                } else {
                    eprintln!("下载后校验失败，触发重试");
                    fs::remove_file(&task.target_path)?;
                }
            }
            Err(e) => {
                eprintln!("下载失败 [{}]: {}", url_used, e);
                if let Some(p) = &progress {
                    p.failed.fetch_add(1, Ordering::SeqCst);
                }
            }
        }

        retry_count += 1;
        if retry_count >= MAX_RETRIES || used_urls.len() >= task.urls.len() {
            break;
        }

        tokio::time::sleep(Duration::from_secs(2)).await; // 增加重试间隔
    }

Err(format!("文件 {} 下载失败，已尝试所有源", task.target_path.display()).into())
}


async fn download_with_url(
    url: &str,
    task: &DownloadTask,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let client = reqwest::Client::new();
    let response = client.get(url)
        .send()
        .await?
        .error_for_status()?;

    // 检查内容长度
    let content_length = response.content_length().unwrap_or(0);
    if task.size > 0 && content_length != task.size {
        return Err(format!("文件大小不匹配: 预期{} 实际{}", task.size, content_length).into());
    }

    // 优化点：使用完整字节缓冲代替流式写入
    let bytes = response.bytes().await?;
    
    // 原子性写入：先写入临时文件，验证后重命名
    let temp_path = task.target_path.with_extension("download");
    {
        let mut file = tokio::fs::File::create(&temp_path).await?;
        file.write_all(&bytes).await?;
        file.sync_all().await?;
    }

    // 校验文件
    let mut file = File::open(&temp_path).await?;
    if !check_sha1(&mut file, &task.sha1).await? {
        return Err("下载后校验失败".into());
    }

    // 原子性重命名
    tokio::fs::rename(&temp_path, &task.target_path).await?;
    
    Ok(())
}


async fn check_sha1(
    file: &mut File,
    expected: &str
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    let mut hasher = Sha1::new();
    let mut buf = vec![0u8; 8192];
    let mut reader = tokio::io::BufReader::new(file);

    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()) == expected)
}

fn calculate_chunk_size(size: u64) -> u64 {
    match size {
        0..=1_000_000 => size,
        1_000_001..=10_000_000 => 1_000_000,
        10_000_001..=100_000_000 => 5_000_000,
        _ => 10_000_000,
    }
}

pub async fn process_version(
    version: &str,
    minecraft_path: &Path,
) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
    let version_dir = minecraft_path.join("versions").join(version);
    fs::create_dir_all(&version_dir)?;
    let natives_dir = &version_dir.join(format!("{}-natives", version));
    fs::create_dir_all(&natives_dir)?;

    let json_url = fetch_version_url(version).await?;
    let is_official = json_url.contains("mojang.com");
    let base_url = if is_official {
        "https://libraries.minecraft.net"
    } else {
        "https://bmclapi2.bangbang93.com/maven"
    };

    let json_content = reqwest::get(&json_url).await?.text().await?;
    fs::write(version_dir.join(format!("{}.json", version)), &json_content)?;
    let version_data: VersionJson = serde_json::from_str(&json_content)?;
     let assets_content = reqwest::get(&version_data.asset_index.url).await?.text().await?;
    
    // 创建assets/indexes目录
    let indexes_dir = minecraft_path.join("assets").join("indexes");
    fs::create_dir_all(&indexes_dir)?;
    
    // 保存索引文件（使用asset_index.id作为文件名）
    let index_path = indexes_dir.join(format!("{}.json", version_data.asset_index.id));
    fs::write(index_path, &assets_content)?;

    let assets_data: AssetsJson = serde_json::from_str(&assets_content)?;


    let mut tasks = Vec::new();

    // 处理客户端JAR
    let client_download = &version_data.downloads.client;
    tasks.push(DownloadTask {
        urls: vec![
            client_download.url.clone(),
            format!("{}/{}/client", MIRROR_URL, version)

        ],
        target_path: version_dir.join(format!("{}.jar", version)),
        sha1: client_download.sha1.clone(),
        size: client_download.size,
    });

    // 处理日志配置文件
    if let Some(logging) = &version_data.logging {
        tasks.push(DownloadTask {
            urls: vec![logging.client.file.url.clone()],
            target_path: version_dir.join("log_config.xml"),
            sha1: logging.client.file.sha1.clone(),
            size: 0,
        });
    }

    // 处理依赖库
    let os_type = if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "osx"
    } else {
        "linux"
    };

    for lib in version_data.libraries {
        if !check_rules(&lib.rules, os_type) {
            continue;
        }

        // 处理主库文件
        if let Some(artifact) = lib.downloads.artifact {
            let mirror_path = artifact.path.replace(
                "https://libraries.minecraft.net/",
                "https://bmclapi2.bangbang93.com/maven/"
            );
            let original_url = format!("{}/{}", base_url, artifact.path); // 新增
            let mirror_url = format!("https://bmclapi2.bangbang93.com/maven/{}", artifact.path);
            let target_path = minecraft_path.join("libraries").join(&artifact.path);
            
            tasks.push(DownloadTask {
                urls: vec![original_url, mirror_url],
                target_path,
                sha1: artifact.sha1,
                size: artifact.size,
            });
        }

        // 处理平台特定库
        if let Some(classifier_template) = lib.natives.get(os_type) {
    let classifier = format!("{}{}", classifier_template, current_arch_suffix());
    if let Some(native) = lib.downloads.classifiers.get(&classifier) {
        // 修正URL拼接方式
        println!("下载平台特定库: {}", native.path);
        let original_url = format!("{}/{}", base_url, native.path);
        let mirror_url = format!("https://bmclapi2.bangbang93.com/maven/{}", native.path);
        let target_path = minecraft_path.join("libraries").join(&native.path);
        
        if match_arch_suffix(&native.path) {
            tasks.push(DownloadTask {
                urls: vec![original_url, mirror_url], // 使用正确拼接的完整URL
                target_path,
                sha1: native.sha1.clone(),
                size: native.size,
            });
        }
    }
}
    }

    // 处理资源文件
    let assets_content = reqwest::get(&version_data.asset_index.url).await?.text().await?;
    let assets_data: AssetsJson = serde_json::from_str(&assets_content)?;

    for (_, obj) in assets_data.objects {
        let prefix = &obj.hash[0..2];
        let original_url = format!("https://resources.download.minecraft.net/{}/{}", prefix, obj.hash);
        let mirror_url = format!("{}/assets/{}/{}", MIRROR_URL, prefix, obj.hash);
        
        tasks.push(DownloadTask {
            urls: vec![original_url, mirror_url],
            target_path: minecraft_path
                .join("assets")
                .join("objects")
                .join(prefix)
                .join(&obj.hash),
            sha1: obj.hash,
            size: 0,
        });
    }

    // 创建进度跟踪器
    let progress = Arc::new(DownloadProgress::new(tasks.len()));
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_DOWNLOADS));
    let mut futures = Vec::new();

    for task in tasks {
        let semaphore = semaphore.clone();
        let progress = progress.clone();
        futures.push(download_task(task, semaphore, Some(progress)));
    }

    let results = stream::iter(futures)
        .buffer_unordered(MAX_CONCURRENT_DOWNLOADS)
        .collect::<Vec<_>>()
        .await;

    for result in results {
        result?;
    }

    println!(
        "下载完成: 成功 {} 失败 {}",
        progress.success.load(Ordering::SeqCst),
        progress.failed.load(Ordering::SeqCst)
    );

    Ok(vec![])
}

fn check_rules(rules: &[Rule], os_type: &str) -> bool {
    let mut allowed = true;
    for rule in rules {
        let os_match = rule.os.as_ref().map_or(true, |os| 
            os.name.as_deref() == Some(os_type)
        );
        match rule.action.as_str() {
            "allow" => allowed = os_match,
            "disallow" => allowed = !os_match,
            _ => ()
        }
    }
    allowed
}

fn current_arch_suffix() -> &'static str {
    match env::consts::ARCH {
        "x86_64" => "",
        "aarch64" => "-arm64",
        "x86" => "-x86",
        "arm" => "-arm",
        _ => "-unknown",
    }
}

fn match_arch_suffix(path: &str) -> bool {
    let file_name = path.split('/').last().unwrap_or_default();
    let expected_suffix = current_arch_suffix();
    match expected_suffix {
        "" => !file_name.contains("-arm64") && !file_name.contains("-x86") && !file_name.contains("-arm"),
        suffix => file_name.contains(suffix)
    }
}

async fn fetch_version_url(version: &str) -> Result<String, Box<dyn Error + Send + Sync>> {
    let client = reqwest::Client::new();
    let response = match client.get(MOJANG_MANIFEST).send().await {
        Ok(res) if res.status().is_success() => res,
        _ => return Ok(format!("{}/{}/json", MIRROR_URL, version)),
    };

    let manifest = response.json::<VersionManifest>().await
        .map_err(|_| format!("Failed to parse version manifest"))?;

    manifest.versions
        .iter()
        .find(|v| v.id == version)
        .map(|e| e.url.clone())
        .or_else(|| manifest.versions.iter()
            .find(|v| v.id.replace(".", "") == version.replace(".", ""))
            .map(|e| e.url.clone()))
        .map(Ok)
        .unwrap_or_else(|| Ok(format!("{}/{}/json", MIRROR_URL, version)))
}

pub async fn download(version: &str, mc_home: &Path) -> Result<()> {
    process_version(version, mc_home)
        .await
        .map(|_| ())
        .map_err(|e| anyhow!(e))?;
    Ok(())
}