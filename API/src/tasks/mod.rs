use std::path::Path;
use tokio::process::{Command};
use std::process::Stdio;
mod version_fetcher;
mod original_dwl;
pub mod launcher;

/// original_dwl 下载任务
pub async fn original_download_task(
    version: &str,
    mc_home: &Path,
) -> anyhow::Result<String> {
    original_dwl::download(version, mc_home).await?;
    Ok(format!("download {version} done"))
}

/// version_fetcher 任务
pub async fn classify_versions_task() -> anyhow::Result<String> {
    let [releases, snapshots, fools, olds] =
        version_fetcher::classify_minecraft_versions().await?;
    Ok(format!(
        "正式版 {:?}\n快照版 {:?}\n愚人节 {:?}\n远古版 {:?}",
        releases, snapshots, fools, olds
    ))
}

pub async fn launch_game_task(
    cfg: launcher::LauncherConfig,
    version: &str,
    player: &str,
    token: &str,
    uuid: &str,
) -> anyhow::Result<String> {
    let args = launcher::build_jvm_arguments(&cfg, version, player, token, uuid)?;
    let status = Command::new(&cfg.java_path)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?
        .wait()
        .await?;
    Ok(format!("game exit: {status}"))
}