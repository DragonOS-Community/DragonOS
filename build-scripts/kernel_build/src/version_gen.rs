use chrono::Utc;
use std::env;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;

#[derive(Debug, serde::Serialize)]
struct KernelBuildInfo {
    pub version: String,
    pub release: String,
    pub build_user: String,
    pub build_host: String,
    pub build_time: String,
    pub compiler_info: String,
    pub linker_info: String,
    pub git_commit: String,
    pub git_branch: String,
    pub config_flags: String,
}

pub(super) fn generate_version() {
    let out_dir = Path::new("./src/init/");
    let version_file = Path::new(&out_dir).join("version_info.rs");

    let build_info = collect_build_info();

    let rust_code = generate_rust_code(&build_info);

    let mut file = File::create(&version_file).unwrap();
    file.write_all(rust_code.as_bytes()).unwrap();

    println!("cargo:rerun-if-changed=../");
}

fn collect_build_info() -> KernelBuildInfo {
    let release = "6.6.21-dragonos".to_string();

    // 检查是否需要更新构建计数
    let is_actual_build = env::var("DRAGONOS_ACTUAL_BUILD").map_or(false, |val| val.trim() == "1");

    // 读取并更新构建计数器
    let build_count = if is_actual_build {
        get_or_update_build_count()
    } else {
        get_build_count(false)
    };

    let build_time = Utc::now().format("%a %b %d %H:%M:%S UTC %Y").to_string();

    let build_user = env::var("USER").unwrap_or_else(|_| "dragonos".to_string());
    let build_host = env::var("HOSTNAME").unwrap_or_else(|_| {
        Command::new("hostname")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "localhost".to_string())
    });
    let kernel_version = env::var("CARGO_PKG_VERSION").unwrap_or("unknown".to_string());
    let version = format!(
        "#{}-dragonos-{} {}",
        build_count, kernel_version, build_time
    );
    let compiler_info = Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.lines().next().unwrap_or("").to_string())
        .unwrap_or_else(|| "rustc (unknown)".to_string());
    let linker_info = Command::new(env::var("LD").unwrap_or_else(|_| "ld".to_string()))
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.lines().next().unwrap_or("").to_string())
        .unwrap_or_else(|| "ld (unknown)".to_string());
    let git_commit = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // 检查是否有未提交的更改
    let is_dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    // 如果有未提交的更改，添加-dirty后缀
    let git_commit = if is_dirty && !git_commit.is_empty() {
        format!("{}-dirty", git_commit)
    } else {
        git_commit
    };
    let git_branch = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let config_flags = env::var("CONFIG_FLAGS").unwrap_or_else(|_| "".to_string());

    KernelBuildInfo {
        version,
        release,
        build_user,
        build_host,
        build_time,
        compiler_info,
        linker_info,
        git_commit,
        git_branch,
        config_flags,
    }
}

fn generate_rust_code(build_info: &KernelBuildInfo) -> String {
    format!(
        r#"
#![allow(dead_code)]
// Auto-generated version information file, do not modify
// Generated at: {}

#[derive(Debug, Clone, Copy)]
pub struct KernelBuildInfo {{
    pub version: &'static str,
    pub release: &'static str,
    pub build_user: &'static str,
    pub build_host: &'static str,
    pub build_time: &'static str,
    pub compiler_info: &'static str,
    pub linker_info: &'static str,
    pub git_commit: &'static str,
    pub git_branch: &'static str,
    pub config_flags: &'static str,
}}

pub static KERNEL_BUILD_INFO: KernelBuildInfo = KernelBuildInfo {{
    version: "{}",
    release: "{}",
    build_user: "{}",
    build_host: "{}",
    build_time: "{}",
    compiler_info: "{}",
    linker_info: "{}",
    git_commit: "{}",
    git_branch: "{}",
    config_flags: "{}",
}};

pub const fn get_kernel_build_info() -> &'static KernelBuildInfo {{
    &KERNEL_BUILD_INFO
}}
"#,
        Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
        build_info.version,
        build_info.release,
        build_info.build_user,
        build_info.build_host,
        build_info.build_time,
        build_info
            .compiler_info
            .replace('\\', "\\\\")
            .replace('\"', "\\\""),
        build_info
            .linker_info
            .replace('\\', "\\\\")
            .replace('\"', "\\\""),
        build_info.git_commit,
        build_info.git_branch,
        build_info.config_flags
    )
}

/// 获取构建计数器（可选择是否更新）
fn get_build_count(increment: bool) -> u32 {
    let counter_file = Path::new(".build_count");

    // 尝试读取现有计数器
    let count = if counter_file.exists() {
        let mut file = File::open(&counter_file).unwrap();
        let mut content = String::new();
        file.read_to_string(&mut content).unwrap();
        content.trim().parse().unwrap_or(0)
    } else {
        0
    };

    if increment {
        // 增加计数器
        let new_count = count + 1;

        // 写入新值
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&counter_file)
            .unwrap();

        file.write_all(new_count.to_string().as_bytes()).unwrap();

        // 将计数器文件添加到 .gitignore（如果还没添加）
        let gitignore_path = Path::new(".gitignore");
        if gitignore_path.exists() {
            let mut gitignore_content = String::new();
            if let Ok(mut gitignore_file) = File::open(gitignore_path) {
                gitignore_file
                    .read_to_string(&mut gitignore_content)
                    .unwrap_or_default();
            }

            if !gitignore_content.lines().any(|line| line == ".build_count") {
                if let Ok(mut gitignore_file) = OpenOptions::new().append(true).open(gitignore_path)
                {
                    writeln!(gitignore_file, "\n# Build counter\n.build_count").unwrap_or_default();
                }
            }
        }

        new_count
    } else {
        // 如果只是读取，返回当前计数+1（用于显示）
        count + 1
    }
}

/// 获取或更新构建计数器
fn get_or_update_build_count() -> u32 {
    get_build_count(true)
}
