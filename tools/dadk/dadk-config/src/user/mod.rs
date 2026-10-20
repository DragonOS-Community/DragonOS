use std::path::PathBuf;

use serde::Deserialize;

use crate::common::{
    target_arch::TargetArch,
    task::{BuildConfig, CleanConfig, Dependency, InstallConfig, TaskEnv, TaskSource},
};

use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserCleanLevel {
    /// 清理所有用户程序构建缓存
    All,
    /// 只在用户程序源码目录下清理
    InSrc,
    /// 只清理用户程序输出目录
    Output,
}

#[derive(Debug, Deserialize, PartialEq)]
/// 用户程序配置文件
pub struct UserConfigFile {
    /// 包名
    pub name: String,
    /// 版本
    pub version: String,
    /// 包的描述
    pub description: String,
    /// 任务类型
    #[serde(rename = "task-source")]
    pub task_source: TaskSource,
    /// 依赖的包
    #[serde(default = "default_empty_dep")]
    pub depends: Vec<Dependency>,
    /// 构建配置
    pub build: BuildConfig,
    /// 安装配置
    pub install: InstallConfig,
    /// 清理配置
    pub clean: CleanConfig,
    /// 环境变量
    #[serde(default = "default_empty_env")]
    pub envs: Vec<TaskEnv>,

    /// (可选) 是否只构建一次，如果为true，DADK会在构建成功后，将构建结果缓存起来，下次构建时，直接使用缓存的构建结果。
    #[serde(rename = "build-once", default = "default_false")]
    pub build_once: bool,

    /// (可选) 是否只安装一次，如果为true，DADK会在安装成功后，不再重复安装。
    #[serde(rename = "install-once", default = "default_false")]
    pub install_once: bool,

    #[serde(rename = "target-arch")]
    pub target_arch: Vec<TargetArch>,
}

impl UserConfigFile {
    pub fn load(path: &PathBuf) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Self::load_from_str(&content)
    }

    pub fn load_from_str(content: &str) -> Result<Self> {
        let config: UserConfigFile = toml::from_str(content)?;
        Ok(config)
    }
}

fn default_empty_env() -> Vec<TaskEnv> {
    vec![]
}

fn default_empty_dep() -> Vec<Dependency> {
    vec![]
}

fn default_false() -> bool {
    false
}
