use std::path::Path;

mod version_fetcher;
mod original_dwl;
mod decompression;

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
