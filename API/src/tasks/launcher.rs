use serde::Deserialize;
use std::{
    collections::{HashSet, HashMap},
    path::PathBuf,
};
use os_info::{Type, Version};
use anyhow::{Context, Result};
use regex::Regex;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VersionJson {
    arguments: Option<Arguments>,
    main_class: String,
    libraries: Vec<Library>,
    #[serde(rename = "inheritsFrom")]
    parent_version: Option<String>,
    logging: Option<Logging>,
    minecraft_arguments: Option<String>,
    asset_index: Option<AssetIndex>,
}

#[derive(Debug, Deserialize)]
struct AssetIndex {
    id: String,
}

#[derive(Debug, Deserialize)]
struct Logging {
    client: Option<LoggingClient>,
}

#[derive(Debug, Deserialize)]
struct LoggingClient {
    file: LogFile,
}

#[derive(Debug, Deserialize)]
struct LogFile {
    id: String,
}

#[derive(Debug, Deserialize)]
struct Arguments {
    jvm: Option<Vec<JvmArgument>>,
    game: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum JvmArgument {
    String(String),
    Object { rules: Vec<Rule>, value: serde_json::Value },
}

#[derive(Debug, Deserialize)]
struct Rule {
    #[serde(rename = "action")]
    action: String,
    #[serde(default)]
    os: Option<OsRule>,
}

#[derive(Debug, Deserialize)]
struct OsRule {
    name: Option<String>,
    arch: Option<String>,
    version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Library {
    name: String,
    downloads: LibraryDownloads,
    #[serde(default)]
    rules: Vec<Rule>,
    #[serde(default)]
    natives: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct LibraryDownloads {
    artifact: Option<Artifact>,
    #[serde(default)]
    classifiers: HashMap<String, Artifact>,
}

#[derive(Debug, Deserialize)]
struct Artifact {
    path: String,
    url: String,
    sha1: String,
    size: u64,
}

pub struct LauncherConfig {
    pub minecraft_path: PathBuf,
    pub java_path: PathBuf,
    pub wrapper_path: PathBuf,
    pub launcher_version: String,
    pub max_memory: String,
    pub version_type: String,
}

pub fn build_jvm_arguments(
    config: &LauncherConfig,
    version_name: &str,
    player_name: &str,
    auth_token: &str,
    uuid: &str,
) -> Result<Vec<String>> {
    // 版本文件路径
    let version_path = config.minecraft_path
        .join("versions")
        .join(version_name)
        .join(format!("{}.json", version_name));
    
    // 解析版本JSON
    let mut version_json: VersionJson = serde_json::from_reader(
        std::fs::File::open(version_path).context("Failed to open version json")?
    ).context("Failed to parse version json")?;

    // 处理继承版本
    if let Some(parent) = &version_json.parent_version {
        let parent_path = config.minecraft_path
            .join("versions")
            .join(parent)
            .join(format!("{}.json", parent));
        
        let parent_json: VersionJson = serde_json::from_reader(
            std::fs::File::open(parent_path).context("Failed to open parent json")?
        )?;
        
        if version_json.asset_index.is_none() {
            version_json.asset_index = parent_json.asset_index;
        }
    }

    // 获取系统信息
    let os_info = os_info::get();
    let is_windows = os_info.os_type() == Type::Windows;
    let is_macos = os_info.os_type() == Type::Macos;
    let is_linux = os_info.os_type() == Type::Linux;

    // 规则检查函数
    fn check_rules(rules: &[Rule], os_info: &os_info::Info) -> bool {
        let mut allowed = true;
        for rule in rules {
            let mut rule_matched = false;
            
            if let Some(os_rule) = &rule.os {
                // 操作系统类型匹配
                let os_match = match os_rule.name.as_deref() {
                    Some("windows") => os_info.os_type() == Type::Windows,
                    Some("osx") => os_info.os_type() == Type::Macos,
                    Some("linux") => os_info.os_type() == Type::Linux,
                    _ => true,
                };
                
                // 操作系统版本匹配
                let version_match = if let Some(version_pattern) = &os_rule.version {
                    let re = Regex::new(version_pattern).unwrap();
                    re.is_match(&os_info.version().to_string())
                } else {
                    true
                };
                
                rule_matched = os_match && version_match;
            }
            
            match rule.action.as_str() {
                "allow" => allowed = rule_matched,
                "disallow" => allowed = !rule_matched,
                _ => ()
            }
        }
        allowed
    }

    // 路径格式化
    let format_path = |p: PathBuf| -> String {
        p.to_string_lossy().replace('\\', "/")
    };

    // 替换
    let replace_placeholders = |s: &str| -> String {
        s.replace("${auth_player_name}", player_name)
         .replace("${auth_session}", uuid)
         .replace("${auth_access_token}", auth_token)
         .replace("${auth_uuid}", uuid)
         .replace("${version_name}", version_name)
         .replace("${natives_directory}", &format_path(
             config.minecraft_path
                 .join("versions")
                 .join(version_name)
                 .join(format!("{}-natives", version_name))
         ))
         .replace("${game_directory}", &format_path(
             config.minecraft_path
                 .join("instance")
                 .join(version_name)
         ))
         .replace("${assets_root}", &format_path(
             config.minecraft_path.join("assets")
         ))
         .replace("${assets_index_name}", 
             &version_json.asset_index.as_ref().map(|a| &a.id).unwrap_or(&String::new()))
         .replace("${user_type}", "msa")
         .replace("${version_type}", config.version_type.as_str())
    };

    // 处理库文件
    let mut class_path_entries: Vec<String> = version_json.libraries
        .iter()
        .filter_map(|lib| {
            if !check_rules(&lib.rules, &os_info) {
                return None;
            }

            let artifact_path = if !lib.downloads.classifiers.is_empty() {
                let classifier = lib.natives.get(match os_info.os_type() {
                    Type::Windows => "windows",
                    Type::Macos => "osx",
                    Type::Linux => "linux",
                    _ => return None,
                }).and_then(|s| s.strip_prefix("natives-"));

                lib.downloads.classifiers.get(classifier?)
                    .map(|a| config.minecraft_path.join("libraries").join(&a.path))
            } else {
                lib.downloads.artifact.as_ref()
                    .map(|a| config.minecraft_path.join("libraries").join(&a.path))
            };

            artifact_path.map(|p| format_path(p))
        })
        .collect();

    // 添加主JAR文件
    let vanilla_jar = format_path(
        config.minecraft_path
            .join("versions")
            .join(version_name)
            .join(format!("{}.jar", version_name))
    );
    class_path_entries.push(vanilla_jar);

    // 构建基础参数
    let mut args = vec![
        "-Xmn768m".to_string(),
        format!("-Xmx{}m", config.max_memory),
    ];

    // 系统相关参数
    if is_macos {
        args.push("-XstartOnFirstThread".to_string());
    }
    if os_info.architecture().map_or(false, |a| a.contains("x86")) {
        args.push("-Xss1M".to_string());
    }
    if is_windows {
        args.push("-XX:HeapDumpPath=MojangTricksIntelDriversForPerformance_javaw.exe_minecraft.exe.heapdump".to_string());
    }

    // 通用JVM参数
    args.extend(vec![
        "-XX:+UseG1GC".to_string(),
        "-XX:-UseAdaptiveSizePolicy".to_string(),
        "-XX:-OmitStackTraceInFastThrow".to_string(),
        "-Djdk.lang.Process.allowAmbiguousCommands=true".to_string(),
        "-Dfml.ignoreInvalidMinecraftCertificates=True".to_string(),
        "-Dfml.ignorePatchDiscrepancies=True".to_string(),
    ]);

    // 日志配置
    if let Some(logging) = &version_json.logging {
        if let Some(client) = &logging.client {
            let log_path = format_path(
                config.minecraft_path
                    .join("versions")
                    .join(version_name)
                    .join(&client.file.id)
            );
        let log_path_encoded = log_path.replace(' ', "%20");   // 仅处理空格即可
        args.push(format!("-Dlog4j.configurationFile=file:///{}", log_path_encoded));
        }
    }

    // 固定参数处理
    let fixed_params = vec![
        format!("-Dos.name={}", os_info.os_type()),
        format!("-Dos.version={}", os_info.version()),
        format!("-Djava.library.path={}", format_path(config.minecraft_path
                 .join("versions")
                 .join(version_name)
                 .join(format!("{}-natives", version_name)))),
        format!("-DlibraryDirectory={}", format_path(config.minecraft_path.join("libraries"))),
        r#"-Dminecraft.launcher.brand="Rhythmic Tool Launcher""#.to_string(),
        format!("-Dminecraft.launcher.version={}", config.launcher_version),
    ];

    let existing_params: HashSet<String> = version_json.arguments
        .iter()
        .flat_map(|a| a.jvm.iter().flatten())
        .filter_map(|arg| match arg {
            JvmArgument::String(s) => Some(s.split('=').next().unwrap().to_string()),
            _ => None,
        })
        .collect();

    for param in fixed_params {
        let key = param.split('=').next().unwrap();
        if !existing_params.contains(key) {
            args.push(param);
        }
    }

    // wrapper-jar加入classpath
    class_path_entries.push(format_path(config.wrapper_path.clone()));

    let class_path = class_path_entries.join(";");
    args.push("-cp".to_string());
    args.push(class_path);

    // 启动主类
    args.push(version_json.main_class);

    // 参数
    if let Some(game_args) = version_json.arguments.as_ref().and_then(|a| a.game.as_ref()) {
        for v in game_args {
            if let Some(s) = v.as_str() {
                args.push(replace_placeholders(s));
            }
        }
    } else if let Some(minecraft_args) = &version_json.minecraft_arguments {
        for arg in minecraft_args.split(' ') {
            args.push(replace_placeholders(arg));
        }
    }

    // 分辨率参数
    args.extend(vec![
        "--width".to_string(),
        "873".to_string(),
        "--height".to_string(),
        "486".to_string(),
    ]);

    let re = Regex::new(r"\$\{[^}]*\}").unwrap();
    Ok(args
        .into_iter()
        .map(|a| re.replace_all(&a, "{}").into_owned())
        .collect())
}