//! # 配置解析器
//!
//! 用于解析配置文件，生成任务列表
//!
//! 您需要指定一个配置文件目录，解析器会自动解析该目录下的所有配置文件。
//! 软件包的配置文件格式为toml
//!
//! ## 简介
//!
//! 在每个配置文件中，您需要指定软件包的名称、版本、描述、任务类型、依赖、构建配置和安装配置。DADK会根据这些信息生成任务列表。
//!
//! ## 配置文件格式
//!
//! ```toml
//! name = "test_git"
//! version = "0.1.0"
//! description = ""
//! build_once = true
//! install_once = true
//! target_arch = ["x86_64"]
//!
//! [task_type]
//! type = "build_from_source"
//! source = "git"
//! source_path = "https://git.mirrors.dragonos.org.cn/DragonOS-Community/test_git.git"
//! revison = "01cdc56863"
//! branch = "test"
//!
//! [build]
//! build-command = "make instal"
//!
//! [install]
//! in_dragonos_path = "/bin"
//!
//! [clean]
//! clean-command = "make clean"
//!
//! [depends]
//! depend1 = "0.1.1"
//! depend2 = "0.1.2"
//!
//! [envs]
//! PATH = "/usr/bin"
//! LD_LIBRARY_PATH = "/usr/lib"

use std::{
    fmt::Debug,
    fs::{DirEntry, ReadDir},
    path::PathBuf,
};

use self::task::DADKTask;
use anyhow::Result;
use dadk_config::{app_blocklist::AppBlocklistConfigFile, user::UserConfigFile};
use log::{debug, error, info, warn};

pub mod task;
pub mod task_log;

/// # 配置解析器
///
/// 用于解析配置文件，生成任务列表
#[derive(Debug)]
pub struct Parser {
    /// 配置文件目录
    config_dir: PathBuf,
    /// 扫描到的配置文件列表
    config_files: Vec<PathBuf>,
    /// 黑名单配置
    blocklist: Option<AppBlocklistConfigFile>,
}

pub struct ParserError {
    pub config_file: Option<PathBuf>,
    pub error: InnerParserError,
}
impl Debug for ParserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.error {
            InnerParserError::IoError(e) => {
                if let Some(config_file) = &self.config_file {
                    write!(
                        f,
                        "IO Error while parsing config file {}: {}",
                        config_file.display(),
                        e
                    )
                } else {
                    write!(f, "IO Error while parsing config files: {}", e)
                }
            }
            InnerParserError::TomlError(e) => {
                if let Some(config_file) = &self.config_file {
                    write!(
                        f,
                        "Toml Error while parsing config file {}: {}",
                        config_file.display(),
                        e
                    )
                } else {
                    write!(f, "Toml Error while parsing config file: {}", e)
                }
            }
            InnerParserError::TaskError(e) => {
                if let Some(config_file) = &self.config_file {
                    write!(
                        f,
                        "Error while parsing config file {}: {}",
                        config_file.display(),
                        e
                    )
                } else {
                    write!(f, "Error while parsing config file: {}", e)
                }
            }
        }
    }
}

#[derive(Debug)]
pub enum InnerParserError {
    IoError(std::io::Error),
    TomlError(toml::de::Error),
    TaskError(String),
}

impl Parser {
    pub fn new(config_dir: PathBuf, blocklist: Option<AppBlocklistConfigFile>) -> Self {
        Self {
            config_dir,
            config_files: Vec::new(),
            blocklist,
        }
    }

    /// # 解析所有配置文件，生成任务列表
    ///
    /// ## 参数
    ///
    /// * `config_dir` - 配置文件所在目录
    ///
    /// ## 返回值
    ///
    /// * `Ok(Vec<(PathBuf, DADKTask)>)` - 任务列表(配置文件路径, 任务)
    /// * `Err(ParserError)` - 解析错误
    pub fn parse(&mut self) -> Result<Vec<(PathBuf, DADKTask)>> {
        self.scan_config_files()?;
        info!("Found {} config files", self.config_files.len());
        let r: Result<Vec<(PathBuf, DADKTask)>> = self.gen_tasks();
        if r.is_err() {
            error!("Error while parsing config files: {:?}", r);
            return r;
        }
        let mut tasks = r.unwrap();

        // 应用黑名单过滤
        self.filter_blocked_apps(&mut tasks)?;

        return Ok(tasks);
    }

    /// # 扫描配置文件目录，找到所有配置文件
    fn scan_config_files(&mut self) -> Result<()> {
        info!("Scanning config files in {}", self.config_dir.display());

        let mut dir_queue: Vec<PathBuf> = Vec::new();
        // 将config目录加入队列
        dir_queue.push(self.config_dir.clone());

        while !dir_queue.is_empty() {
            // 扫描目录，找到所有*.dadk文件
            let dir = dir_queue.pop().unwrap();
            let entries: ReadDir = std::fs::read_dir(&dir)?;

            for entry in entries {
                let entry: DirEntry = entry?;

                let path: PathBuf = entry.path();
                if path.is_dir() {
                    dir_queue.push(path);
                } else if path.is_file() {
                    let extension: Option<&std::ffi::OsStr> = path.extension();
                    if extension.is_none() {
                        continue;
                    }
                    let extension: &std::ffi::OsStr = extension.unwrap();
                    if extension.to_ascii_lowercase() != "toml" {
                        continue;
                    }
                    // 找到一个配置文件, 加入列表
                    self.config_files.push(path);
                }
            }
        }

        return Ok(());
    }

    /// # 解析所有配置文件，生成任务列表
    ///
    /// 一旦发生错误，立即返回
    ///
    /// ## 返回值
    ///
    /// * `Ok(Vec<DADKTask>)` - 任务列表
    /// * `Err(ParserError)` - 解析错误
    fn gen_tasks(&self) -> Result<Vec<(PathBuf, DADKTask)>> {
        let mut result_vec = Vec::new();
        for config_file in &self.config_files {
            let task: DADKTask = self.parse_config_file(config_file)?;
            debug!("Parsed config file {}: {:?}", config_file.display(), task);
            result_vec.push((config_file.clone(), task));
        }

        return Ok(result_vec);
    }

    /// # 解析单个配置文件，生成任务
    ///
    /// ## 参数
    ///
    /// * `config_file` - 配置文件路径
    ///
    /// ## 返回值
    ///
    /// * `Ok(DADKTask)` - 生成好的任务
    /// * `Err(ParserError)` - 解析错误
    pub(super) fn parse_config_file(&self, config_file: &PathBuf) -> Result<DADKTask> {
        log::trace!("Parsing config file {}", config_file.display());
        // 从toml文件中解析出DADKTask
        let mut task: DADKTask = Self::parse_toml_file(config_file)?;

        // 去除字符串中的空白字符
        task.trim();

        // 校验DADKTask的参数是否合法
        task.validate()?;

        return Ok(task);
    }

    /// 解析toml文件，生成DADKTask
    pub fn parse_toml_file(config_file: &PathBuf) -> Result<DADKTask> {
        let dadk_user_config = UserConfigFile::load(config_file)?;
        DADKTask::try_from(dadk_user_config)
    }

    /// 过滤黑名单中的应用程序
    fn filter_blocked_apps(&self, tasks: &mut Vec<(PathBuf, DADKTask)>) -> Result<()> {
        let blocklist = match &self.blocklist {
            Some(blocklist) if blocklist.blocked_count() > 0 => blocklist,
            _ => return Ok(()),
        };

        if blocklist.log_skipped {
            info!(
                "Found {} applications in blocklist",
                blocklist.blocked_count()
            );
        }

        let mut skipped_apps = Vec::new();
        let mut remaining_tasks = Vec::new();

        // 过滤任务
        for (config_path, task) in tasks.drain(..) {
            if blocklist.is_blocked(&task.name, Some(&task.version)) {
                skipped_apps.push(task.name.clone());

                if blocklist.log_skipped {
                    if blocklist.strict {
                        warn!(
                            "Skipping blocked application '{}' (config: {})",
                            task.name,
                            config_path.display()
                        );
                    } else {
                        warn!(
                            "Application '{}' is in blocklist but not skipped (strict mode off)",
                            task.name
                        );
                    }
                }

                // 只有在严格模式下才真正跳过
                if blocklist.strict {
                    continue;
                }
            }
            remaining_tasks.push((config_path, task));
        }

        *tasks = remaining_tasks;

        // 输出摘要信息
        if !skipped_apps.is_empty() && blocklist.log_skipped {
            if blocklist.strict {
                info!(
                    "Skipped {} blocked applications: {}",
                    skipped_apps.len(),
                    skipped_apps.join(", ")
                );
            } else {
                info!(
                    "Found {} applications in blocklist but not skipped (strict mode off): {}",
                    skipped_apps.len(),
                    skipped_apps.join(", ")
                );
            }

            // 输出所有blocked的应用
            for (idx, app) in skipped_apps.iter().enumerate() {
                log::debug!("Blocked application {}: {}", idx + 1, app);
            }
        }

        Ok(())
    }
}
