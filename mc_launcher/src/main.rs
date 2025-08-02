use std::{path::PathBuf, process::Command};

mod launcher;

fn main() -> anyhow::Result<()> {
    let config = launcher::LauncherConfig {
        minecraft_path: PathBuf::from("C:/Users/smh20/.minecraftx"),
        java_path: PathBuf::from("C:/Program Files/Java/jdk-17/bin/java.exe"),
        wrapper_path: PathBuf::from("C:/Users/smh20/Documents/Rust/mc_launcher/java_launch_wrapper-1.4.3.jar"),
        launcher_version: "1.0.0".to_string(),
        max_memory: "16384".to_string(),
    };

    let args = launcher::build_jvm_arguments(
        &config,
        "1.19.2",
        "0xBD",
        "notch",
        "foreverwithyou",
    )?;

    let mut cmd = Command::new(&config.java_path);
    cmd.args(&args);

    let mut child = cmd.spawn()?;
    let status = child.wait()?;
    println!("Minecraft exited with status: {status}");
    Ok(())
}
