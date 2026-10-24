use anyhow::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct TaskSource {
    #[serde(rename = "type")]
    pub source_type: TaskSourceType,
    pub source: Source,
    #[serde(rename = "source-path")]
    pub source_path: String,
    /// 把压缩包中的哪个目录作为根目录（可选）
    ///
    /// 仅当 source = "archive" 时生效
    #[serde(rename = "archive-rootdir")]
    pub archive_rootdir: Option<String>,
    /// 分支（可选，如果为空，则拉取master）branch和revision只能同时指定一个
    pub branch: Option<String>,
    /// 特定的提交的hash值（可选，如果为空，则拉取branch的最新提交）
    pub revision: Option<String>,
}

/// # 任务类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TaskSourceType {
    /// 从源码构建
    #[serde(rename = "build-from-source")]
    BuildFromSource,
    /// 从预编译包安装
    #[serde(rename = "install-from-prebuilt")]
    InstallFromPrebuilt,
}

/// # 来源类型
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub enum Source {
    /// 从Git仓库获取
    #[serde(rename = "git")]
    Git,
    /// 从本地目录获取
    #[serde(rename = "local")]
    Local,
    /// 从在线压缩包获取
    #[serde(rename = "archive")]
    Archive,
}

/// @brief 构建配置
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BuildConfig {
    /// 构建命令
    #[serde(rename = "build-command")]
    pub build_command: Option<String>,
    /// 构建前执行的脚本
    #[serde(rename = "pre-build")]
    pub pre_build: Option<PathBuf>,
    #[serde(rename = "post-build")]
    /// 构建后执行的脚本
    pub post_build: Option<PathBuf>,
}

impl BuildConfig {
    #[allow(dead_code)]
    pub fn new(
        build_command: Option<String>,
        pre_build: Option<PathBuf>,
        post_build: Option<PathBuf>,
    ) -> Self {
        Self {
            build_command,
            pre_build,
            post_build,
        }
    }

    pub fn validate(&self) -> Result<()> {
        return Ok(());
    }

    pub fn trim(&mut self) {
        if let Some(build_command) = &mut self.build_command {
            *build_command = build_command.trim().to_string();
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstallConfig {
    /// 安装到DragonOS内的目录
    #[serde(rename = "in-dragonos-path")]
    pub in_dragonos_path: Option<PathBuf>,
}

impl InstallConfig {
    #[allow(dead_code)]
    pub fn new(in_dragonos_path: Option<PathBuf>) -> Self {
        Self { in_dragonos_path }
    }

    pub fn validate(&self) -> Result<()> {
        if self.in_dragonos_path.is_none() {
            return Ok(());
        }
        if self.in_dragonos_path.as_ref().unwrap().is_relative() {
            return Err(Error::msg(
                "InstallConfig: in_dragonos_path should be an Absolute path",
            ));
        }
        return Ok(());
    }

    pub fn trim(&mut self) {}
}
/// # 清理配置
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CleanConfig {
    /// 清理命令
    #[serde(rename = "clean-command")]
    pub clean_command: Option<String>,
}

impl CleanConfig {
    #[allow(dead_code)]
    pub fn new(clean_command: Option<String>) -> Self {
        Self { clean_command }
    }

    pub fn validate(&self) -> Result<()> {
        return Ok(());
    }

    pub fn trim(&mut self) {
        if let Some(clean_command) = &mut self.clean_command {
            *clean_command = clean_command.trim().to_string();
        }
    }
}

/// @brief 依赖项
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Dependency {
    #[serde(default = "default_empty_string")]
    pub name: String,
    #[serde(default = "default_empty_string")]
    pub version: String,
}

impl Dependency {
    #[allow(dead_code)]
    pub fn new(name: String, version: String) -> Self {
        Self { name, version }
    }

    pub fn validate(&self) -> Result<()> {
        if self.name.is_empty() {
            return Err(Error::msg("name is empty"));
        }
        if self.version.is_empty() {
            return Err(Error::msg("version is empty"));
        }
        return Ok(());
    }

    pub fn trim(&mut self) {
        self.name = self.name.trim().to_string();
        self.version = self.version.trim().to_string();
    }

    pub fn name_version(&self) -> String {
        return format!("{}-{}", self.name, self.version);
    }
}

/// # 任务环境变量
///
/// 任务执行时的环境变量.这个环境变量是在当前任务执行时设置的，不会影响到其他任务
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct TaskEnv {
    #[serde(default = "default_empty_string")]
    pub key: String,
    #[serde(default = "default_empty_string")]
    pub value: String,
}

impl TaskEnv {
    #[allow(dead_code)]
    pub fn new(key: String, value: String) -> Self {
        Self { key, value }
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn trim(&mut self) {
        self.key = self.key.trim().to_string();
        self.value = self.value.trim().to_string();
    }

    pub fn validate(&self) -> Result<()> {
        if self.key.is_empty() {
            return Err(Error::msg("Env: key is empty"));
        }
        return Ok(());
    }
}

fn default_empty_string() -> String {
    "".to_string()
}
