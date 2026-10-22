use std::path::PathBuf;

use crate::executor::source::{ArchiveSource, GitSource, LocalSource};
use dadk_config::{
    common::{
        target_arch::TargetArch,
        task::{
            BuildConfig, CleanConfig, Dependency, InstallConfig, Source, TaskEnv, TaskSource,
            TaskSourceType,
        },
    },
    user::UserConfigFile,
};
use serde::{Deserialize, Serialize};

use anyhow::{Ok, Result};

// 对于生成的包名和版本号，需要进行替换的字符。
pub static NAME_VERSION_REPLACE_TABLE: [(&str, &str); 6] = [
    (" ", "_"),
    ("\t", "_"),
    ("-", "_"),
    (".", "_"),
    ("+", "_"),
    ("*", "_"),
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DADKTask {
    /// 包名
    pub name: String,
    /// 版本
    pub version: String,
    /// 包的描述
    pub description: String,
    /// 任务类型
    pub task_type: TaskType,
    /// 依赖的包
    pub depends: Vec<Dependency>,
    /// 构建配置
    pub build: BuildConfig,
    /// 安装配置
    pub install: InstallConfig,
    /// 清理配置
    pub clean: CleanConfig,
    /// 环境变量
    pub envs: Option<Vec<TaskEnv>>,

    /// (可选) 是否只构建一次，如果为true，DADK会在构建成功后，将构建结果缓存起来，下次构建时，直接使用缓存的构建结果。
    #[serde(default)]
    pub build_once: bool,

    /// (可选) 是否只安装一次，如果为true，DADK会在安装成功后，不再重复安装。
    #[serde(default)]
    pub install_once: bool,

    #[serde(default = "DADKTask::default_target_arch_vec")]
    pub target_arch: Vec<TargetArch>,
}

impl DADKTask {
    #[allow(dead_code)]
    pub fn new(
        name: String,
        version: String,
        description: String,
        task_type: TaskType,
        depends: Vec<Dependency>,
        build: BuildConfig,
        install: InstallConfig,
        clean: CleanConfig,
        envs: Option<Vec<TaskEnv>>,
        build_once: bool,
        install_once: bool,
        target_arch: Option<Vec<TargetArch>>,
    ) -> Self {
        Self {
            name,
            version,
            description,
            task_type,
            depends,
            build,
            install,
            clean,
            envs,
            build_once,
            install_once,
            target_arch: target_arch.unwrap_or_else(Self::default_target_arch_vec),
        }
    }

    /// 默认的目标处理器架构
    ///
    /// 从环境变量`ARCH`中获取，如果没有设置，则默认为`x86_64`
    pub fn default_target_arch() -> TargetArch {
        let s = std::env::var("ARCH").unwrap_or("x86_64".to_string());
        return TargetArch::try_from(s.as_str()).unwrap();
    }

    fn default_target_arch_vec() -> Vec<TargetArch> {
        vec![Self::default_target_arch()]
    }

    pub fn validate(&mut self) -> Result<()> {
        if self.name.is_empty() {
            return Err(anyhow::Error::msg("name is empty"));
        }
        if self.version.is_empty() {
            return Err(anyhow::Error::msg("version is empty"));
        }
        self.task_type.validate()?;
        self.build.validate()?;
        self.validate_build_type()?;
        self.install.validate()?;
        self.clean.validate()?;
        self.validate_depends()?;
        self.validate_envs()?;
        self.validate_target_arch()?;

        return Ok(());
    }

    pub fn trim(&mut self) {
        self.name = self.name.trim().to_string();
        self.version = self.version.trim().to_string();
        self.description = self.description.trim().to_string();
        self.task_type.trim();
        self.build.trim();
        self.install.trim();
        self.clean.trim();
        self.trim_depends();
        self.trim_envs();
    }

    fn validate_depends(&self) -> Result<()> {
        for depend in &self.depends {
            depend.validate()?;
        }
        return Ok(());
    }

    fn trim_depends(&mut self) {
        for depend in &mut self.depends {
            depend.trim();
        }
    }

    fn validate_envs(&self) -> Result<()> {
        if let Some(envs) = &self.envs {
            for env in envs {
                env.validate()?;
            }
        }
        return Ok(());
    }

    fn validate_target_arch(&self) -> Result<()> {
        if self.target_arch.is_empty() {
            return Err(anyhow::Error::msg("target_arch is empty"));
        }
        return Ok(());
    }

    fn trim_envs(&mut self) {
        if let Some(envs) = &mut self.envs {
            for env in envs {
                env.trim();
            }
        }
    }

    /// 验证任务类型与构建配置是否匹配
    fn validate_build_type(&self) -> Result<()> {
        match &self.task_type {
            TaskType::BuildFromSource(_) => {
                if self.build.build_command.is_none() {
                    return Err(anyhow::Error::msg("build command is empty"));
                }
            }
            TaskType::InstallFromPrebuilt(_) => {
                if self.build.build_command.is_some() {
                    return Err(anyhow::Error::msg(
                        "build command should be empty when install from prebuilt",
                    ));
                }
            }
        }
        return Ok(());
    }

    pub fn name_version(&self) -> String {
        let mut name_version = format!("{}-{}", self.name, self.version);
        for (src, dst) in &NAME_VERSION_REPLACE_TABLE {
            name_version = name_version.replace(src, dst);
        }
        return name_version;
    }

    pub fn name_version_env(&self) -> String {
        return Self::name_version_uppercase(&self.name, &self.version);
    }

    pub fn name_version_uppercase(name: &str, version: &str) -> String {
        let mut name_version = format!("{}-{}", name, version).to_ascii_uppercase();
        for (src, dst) in &NAME_VERSION_REPLACE_TABLE {
            name_version = name_version.replace(src, dst);
        }
        return name_version;
    }

    /// # 获取源码目录
    ///
    /// 如果从本地路径构建，则返回本地路径。否则返回None。
    pub fn source_path(&self) -> Option<PathBuf> {
        match &self.task_type {
            TaskType::BuildFromSource(cs) => match cs {
                CodeSource::Local(lc) => {
                    return Some(lc.path().clone());
                }
                _ => {
                    return None;
                }
            },
            TaskType::InstallFromPrebuilt(ps) => match ps {
                PrebuiltSource::Local(lc) => {
                    return Some(lc.path().clone());
                }
                _ => {
                    return None;
                }
            },
        }
    }
}

impl TryFrom<UserConfigFile> for DADKTask {
    type Error = anyhow::Error;

    fn try_from(user_config: UserConfigFile) -> Result<Self> {
        Ok(DADKTask {
            name: user_config.name,
            version: user_config.version,
            description: user_config.description,
            task_type: TaskType::try_from(user_config.task_source)?,
            depends: user_config.depends,
            build: user_config.build,
            install: user_config.install,
            clean: user_config.clean,
            envs: Some(user_config.envs),
            build_once: user_config.build_once,
            install_once: user_config.install_once,
            target_arch: user_config.target_arch,
        })
    }
}

/// # 任务类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TaskType {
    /// 从源码构建
    BuildFromSource(CodeSource),
    /// 从预编译包安装
    InstallFromPrebuilt(PrebuiltSource),
}

impl TaskType {
    pub fn validate(&mut self) -> Result<()> {
        match self {
            TaskType::BuildFromSource(source) => source.validate(),
            TaskType::InstallFromPrebuilt(source) => source.validate(),
        }
    }

    pub fn trim(&mut self) {
        match self {
            TaskType::BuildFromSource(source) => source.trim(),
            TaskType::InstallFromPrebuilt(source) => source.trim(),
        }
    }
}

impl TryFrom<TaskSource> for TaskType {
    type Error = anyhow::Error;
    fn try_from(task_source: TaskSource) -> Result<Self> {
        match task_source.source_type {
            TaskSourceType::BuildFromSource => match task_source.source {
                Source::Git => Ok(TaskType::BuildFromSource(CodeSource::Git(GitSource::new(
                    task_source.source_path,
                    task_source.branch,
                    task_source.revision,
                )))),
                Source::Local => Ok(TaskType::BuildFromSource(CodeSource::Local(
                    LocalSource::new(PathBuf::from(task_source.source_path)),
                ))),
                Source::Archive => Ok(TaskType::BuildFromSource(CodeSource::Archive(
                    ArchiveSource::new(task_source.source_path, task_source.archive_rootdir),
                ))),
            },
            TaskSourceType::InstallFromPrebuilt => match task_source.source {
                Source::Git => Err(anyhow::Error::msg(
                    "InstallFromPrebuild doesn't support Git",
                )),
                Source::Local => Ok(TaskType::InstallFromPrebuilt(PrebuiltSource::Local(
                    LocalSource::new(PathBuf::from(task_source.source_path)),
                ))),
                Source::Archive => Ok(TaskType::InstallFromPrebuilt(PrebuiltSource::Archive(
                    ArchiveSource::new(task_source.source_path, task_source.archive_rootdir),
                ))),
            },
        }
    }
}

/// # 代码源
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CodeSource {
    /// 从Git仓库获取
    Git(GitSource),
    /// 从本地目录获取
    Local(LocalSource),
    /// 从在线压缩包获取
    Archive(ArchiveSource),
}

impl CodeSource {
    pub fn validate(&mut self) -> Result<()> {
        match self {
            CodeSource::Git(source) => source.validate(),
            CodeSource::Local(source) => source.validate(Some(false)),
            CodeSource::Archive(source) => source.validate(),
        }
    }
    pub fn trim(&mut self) {
        match self {
            CodeSource::Git(source) => source.trim(),
            CodeSource::Local(source) => source.trim(),
            CodeSource::Archive(source) => source.trim(),
        }
    }
}

/// # 预编译包源
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PrebuiltSource {
    /// 从在线压缩包获取
    Archive(ArchiveSource),
    /// 从本地目录/文件获取
    Local(LocalSource),
}

impl PrebuiltSource {
    pub fn validate(&self) -> Result<()> {
        match self {
            PrebuiltSource::Archive(source) => source.validate(),
            PrebuiltSource::Local(source) => source.validate(None),
        }
    }

    pub fn trim(&mut self) {
        match self {
            PrebuiltSource::Archive(source) => source.trim(),
            PrebuiltSource::Local(source) => source.trim(),
        }
    }
}
