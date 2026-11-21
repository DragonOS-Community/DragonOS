use std::{
    collections::{BTreeMap, VecDeque},
    env::Vars,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Arc, RwLock},
    time::SystemTime,
};

use chrono::{DateTime, Utc};
use dadk_config::user::UserCleanLevel;
use log::{debug, error, info, warn};

use crate::{
    context::{Action, DadkUserExecuteContext},
    executor::cache::CacheDir,
    parser::{
        task::{CodeSource, PrebuiltSource, TaskType},
        task_log::{BuildStatus, InstallStatus, TaskLog},
    },
    scheduler::{SchedEntities, SchedEntity},
    utils::{file::FileUtils, path::abs_path},
};

use dadk_config::common::task::TaskEnv;

use self::cache::{CacheDirType, TaskDataDir};

pub mod cache;
pub mod source;
#[cfg(test)]
mod tests;

lazy_static! {
    // 全局环境变量的列表
    pub static ref ENV_LIST: RwLock<EnvMap> = RwLock::new(EnvMap::new());
}

#[derive(Debug, Clone)]
pub struct Executor {
    entity: Arc<SchedEntity>,
    action: Action,
    local_envs: EnvMap,
    /// 任务构建结果输出到的目录
    build_dir: CacheDir,
    /// 如果任务需要源文件缓存，则此字段为 Some(CacheDir)，否则为 None（使用本地源文件路径）
    source_dir: Option<CacheDir>,
    /// 任务数据目录
    task_data_dir: TaskDataDir,
    /// DragonOS sysroot的路径
    dragonos_sysroot: PathBuf,
}

impl Executor {
    /// # 创建执行器
    ///
    /// 用于执行一个任务
    ///
    /// ## 参数
    ///
    /// * `entity` - 任务调度实体
    ///
    /// ## 返回值
    ///
    /// * `Ok(Executor)` - 创建成功
    /// * `Err(ExecutorError)` - 创建失败
    pub fn new(
        entity: Arc<SchedEntity>,
        action: Action,
        dragonos_sysroot: PathBuf,
    ) -> Result<Self, ExecutorError> {
        let local_envs = EnvMap::new();
        let build_dir = CacheDir::new(entity.clone(), CacheDirType::Build)?;
        let task_data_dir = TaskDataDir::new(entity.clone())?;

        let source_dir = if CacheDir::need_source_cache(&entity) {
            Some(CacheDir::new(entity.clone(), CacheDirType::Source)?)
        } else {
            None
        };

        let result: Executor = Self {
            action,
            entity,
            local_envs,
            build_dir,
            source_dir,
            task_data_dir,
            dragonos_sysroot,
        };

        return Ok(result);
    }

    /// # 执行任务
    ///
    /// 创建执行器后，调用此方法执行任务。
    /// 该方法会执行以下步骤：
    ///
    /// 1. 创建工作线程
    /// 2. 准备环境变量
    /// 3. 拉取数据（可选）
    /// 4. 执行构建
    pub fn execute(&mut self) -> Result<(), ExecutorError> {
        info!("Execute task: {}", self.entity.task().name_version());

        let r = self.do_execute();
        self.save_task_data(r.clone());
        info!("Task {} finished", self.entity.task().name_version());
        return r;
    }

    /// # 保存任务数据
    fn save_task_data(&self, r: Result<(), ExecutorError>) {
        let mut task_log = self.task_data_dir.task_log();
        match self.action {
            Action::Build => {
                if r.is_ok() {
                    task_log.set_build_status(BuildStatus::Success);
                } else {
                    task_log.set_build_status(BuildStatus::Failed);
                }

                task_log.set_build_time_now();
            }

            Action::Install => {
                if r.is_ok() {
                    task_log.set_install_status(InstallStatus::Success);
                } else {
                    task_log.set_install_status(InstallStatus::Failed);
                }
                task_log.set_install_time_now();
            }

            Action::Clean(_) => {
                task_log.clean_build_status();
                task_log.clean_install_status();
            }
        }

        // 设置dadk配置文件的时间戳
        if let Some(ts) = self.entity.config_file_timestamp() {
            task_log.set_dadk_config_timestamp(ts);
        }

        self.task_data_dir
            .save_task_log(&task_log)
            .expect("Failed to save task log");
    }

    fn do_execute(&mut self) -> Result<(), ExecutorError> {
        // 准备本地环境变量
        self.prepare_local_env()?;

        match self.action {
            Action::Build => {
                // 构建前的工作
                self.pre_build()?;
                // 构建任务
                self.build()?;
                // 构建完毕后的工作
                self.post_build()?;
            }
            Action::Install => {
                // 把构建结果安装到DragonOS
                self.install()?;
            }
            Action::Clean(_) => {
                // 清理构建结果
                let r = self.clean();
                if let Err(e) = r {
                    error!(
                        "Failed to clean task {}: {:?}",
                        self.entity.task().name_version(),
                        e
                    );
                }
            }
        }

        return Ok(());
    }

    fn pre_build(&mut self) -> Result<(), ExecutorError> {
        if let Some(pre_build) = self.entity.task().build.pre_build {
            let output = Command::new(pre_build)
                .output()
                .expect("Failed to execute pre_build script");

            // 检查脚本执行结果
            if output.status.success() {
                info!("Pre-build script executed successfully");
            } else {
                error!("Pre-build script failed");
                return Err(ExecutorError::TaskFailed(
                    "Pre-build script failed".to_string(),
                ));
            }
        }
        Ok(())
    }

    fn needs_build(&self) -> Result<bool, ExecutorError> {
        // dadk配置文件更新了，需要重新构建
        if self.config_file_updated() {
            return Ok(true);
        }
        let task_log = self.task_log();
        let build_status = task_log.build_status();
        if build_status.is_none() {
            return Ok(true);
        }

        let build_time = task_log.build_time();
        if build_time.is_none() {
            return Ok(true);
        }

        let build_status = build_status.unwrap();
        let build_time = build_time.unwrap();

        let mut last_modified = last_modified_time(&self.entity.file_path(), build_time)?;
        if let Some(ref d) = self.src_work_dir() {
            last_modified = core::cmp::max(last_modified, last_modified_time(d, build_time)?);
        }

        // 跳过构建
        if *build_status == BuildStatus::Success
            && (self.entity.task().build_once || last_modified < *build_time)
        {
            return Ok(false);
        }

        return Ok(true);
    }

    fn config_file_updated(&self) -> bool {
        let task_log = self.task_log();
        task_log.dadk_config_timestamp() != self.entity.config_file_timestamp().as_ref()
    }

    fn build(&mut self) -> Result<(), ExecutorError> {
        if !self.needs_build().unwrap_or(true) {
            log::info!(
                "No need to build: {}, skipping...",
                self.entity.task().name_version()
            );
            return Ok(());
        }

        return self.do_build();
    }

    fn post_build(&mut self) -> Result<(), ExecutorError> {
        if let Some(post_build) = self.entity.task().build.post_build {
            let output = Command::new(post_build)
                .output()
                .expect("Failed to execute post_build script");

            // 检查脚本执行结果
            if output.status.success() {
                info!("Post-build script executed successfully");
            } else {
                error!("Post-build script failed");
                return Err(ExecutorError::TaskFailed(
                    "Post-build script failed".to_string(),
                ));
            }
        }
        Ok(())
    }

    /// # 执行build操作
    fn do_build(&mut self) -> Result<(), ExecutorError> {
        // 确认源文件就绪
        self.prepare_input()?;

        let command: Option<Command> = self.create_command()?;
        if let Some(cmd) = command {
            self.run_command(cmd)?;
        }

        // 检查构建结果，如果为空，则抛出警告
        if self.build_dir.is_empty()? {
            warn!(
                "Task {}: build result is empty, do you forget to copy the result to [$DADK_CURRENT_BUILD_DIR]?",
                self.entity.task().name_version(),
            );
        }
        return Ok(());
    }

    fn needs_install(&self) -> Result<bool, ExecutorError> {
        // dadk配置文件更新了，需要重新安装
        if self.config_file_updated() {
            return Ok(true);
        }
        let task_log = self.task_log();
        let install_status = task_log.install_status();
        if install_status.is_none() {
            return Ok(true);
        }

        let install_time = task_log.install_time();
        if install_time.is_none() {
            return Ok(true);
        }

        let install_status = install_status.unwrap();
        let install_time = install_time.unwrap();

        let last_modified = last_modified_time(&self.build_dir.path, install_time)?;
        let last_modified = core::cmp::max(
            last_modified,
            last_modified_time(&self.entity.file_path(), install_time)?,
        );

        // 跳过构建
        if *install_status == InstallStatus::Success
            && (self.entity.task().install_once || last_modified < *install_time)
        {
            return Ok(false);
        }

        return Ok(true);
    }

    fn install(&self) -> Result<(), ExecutorError> {
        log::trace!("dadk-user: install {}", self.entity.task().name_version());
        if !self.needs_install().unwrap_or(false) {
            info!(
                "install: Task {} not changed.",
                self.entity.task().name_version()
            );
            return Ok(());
        }
        log::trace!(
            "dadk-user: to do install {}",
            self.entity.task().name_version()
        );
        return self.do_install();
    }

    /// # 执行安装操作，把构建结果安装到DragonOS
    fn do_install(&self) -> Result<(), ExecutorError> {
        let binding = self.entity.task();
        let in_dragonos_path = binding.install.in_dragonos_path.as_ref();
        // 如果没有指定安装路径，则不执行安装
        if in_dragonos_path.is_none() {
            return Ok(());
        }
        info!("Installing task: {}", self.entity.task().name_version());
        let mut in_dragonos_path = in_dragonos_path.unwrap().to_string_lossy().to_string();

        debug!("in_dragonos_path: {}", in_dragonos_path);
        // 去除开头的斜杠
        {
            let count_leading_slashes = in_dragonos_path.chars().take_while(|c| *c == '/').count();
            in_dragonos_path = in_dragonos_path[count_leading_slashes..].to_string();
        }
        // 拼接最终的安装路径
        let install_path = abs_path(&self.dragonos_sysroot.join(in_dragonos_path));
        debug!("install_path: {:?}", install_path);
        // 创建安装路径
        std::fs::create_dir_all(&install_path).map_err(|e| {
            ExecutorError::InstallError(format!("Failed to create install path: {}", e.to_string()))
        })?;

        // 拷贝构建结果到安装路径
        let build_dir: PathBuf = self.build_dir.path.clone();
        FileUtils::copy_dir_all(&build_dir, &install_path)
            .map_err(|e| ExecutorError::InstallError(e))?;
        info!("Task {} installed.", self.entity.task().name_version());

        return Ok(());
    }

    fn clean(&self) -> Result<(), ExecutorError> {
        let level = if let Action::Clean(l) = self.action {
            l
        } else {
            panic!(
                "BUG: clean() called with non-clean action. executor details: {:?}",
                self
            );
        };
        info!(
            "Cleaning task: {}, level={level:?}",
            self.entity.task().name_version()
        );

        let r: Result<(), ExecutorError> = match level {
            UserCleanLevel::All => self.clean_all(),
            UserCleanLevel::InSrc => self.clean_src(),
            UserCleanLevel::Output => {
                self.clean_target()?;
                self.clean_cache()
            }
        };

        if let Err(e) = r {
            error!(
                "Failed to clean task: {}, error message: {:?}",
                self.entity.task().name_version(),
                e
            );
            return Err(e);
        }

        return Ok(());
    }

    fn clean_all(&self) -> Result<(), ExecutorError> {
        // 在源文件目录执行清理
        self.clean_src()?;
        // 清理构建结果
        self.clean_target()?;
        // 清理缓存
        self.clean_cache()?;
        return Ok(());
    }

    /// 在源文件目录执行清理
    fn clean_src(&self) -> Result<(), ExecutorError> {
        let cmd: Option<Command> = self.create_command()?;
        if cmd.is_none() {
            log::warn!(
                "{}: No clean command specified, skipping source directory clean.",
                self.entity.task().name_version()
            );
            // 如果这里没有命令，则认为用户不需要在源文件目录执行清理
            return Ok(());
        }
        info!(
            "{}: Cleaning in source directory: {:?}",
            self.entity.task().name_version(),
            self.src_work_dir()
        );

        let cmd = cmd.unwrap();
        self.run_command(cmd)?;
        return Ok(());
    }

    /// 清理构建输出目录
    fn clean_target(&self) -> Result<(), ExecutorError> {
        info!(
            "{}: Cleaning build target directory: {:?}",
            self.entity.task().name_version(),
            self.build_dir.path
        );

        return self.build_dir.remove_self_recursive();
    }

    /// 清理下载缓存
    fn clean_cache(&self) -> Result<(), ExecutorError> {
        let cache_dir = self.source_dir.as_ref();
        if cache_dir.is_none() {
            // 如果没有缓存目录，则认为用户不需要清理缓存
            return Ok(());
        }
        info!(
            "{}: Cleaning cache directory: {}",
            self.entity.task().name_version(),
            cache_dir.as_ref().unwrap().path.display()
        );
        return cache_dir.unwrap().remove_self_recursive();
    }

    /// 获取源文件的工作目录
    fn src_work_dir(&self) -> Option<PathBuf> {
        if let Some(local_path) = self.entity.task().source_path() {
            return Some(local_path);
        }
        return Some(self.source_dir.as_ref()?.path.clone());
    }

    fn task_log(&self) -> TaskLog {
        return self.task_data_dir.task_log();
    }

    /// 为任务创建命令
    fn create_command(&self) -> Result<Option<Command>, ExecutorError> {
        // 获取命令
        let raw_cmd = match self.entity.task().task_type {
            TaskType::BuildFromSource(_) => match self.action {
                Action::Build => self.entity.task().build.build_command.clone(),
                Action::Clean(_) => self.entity.task().clean.clean_command.clone(),
                _ => unimplemented!(
                    "create_command: Action {:?} not supported yet.",
                    self.action
                ),
            },

            TaskType::InstallFromPrebuilt(_) => match self.action {
                Action::Build => self.entity.task().build.build_command.clone(),
                Action::Clean(_) => self.entity.task().clean.clean_command.clone(),
                _ => unimplemented!(
                    "create_command: Action {:?} not supported yet.",
                    self.action
                ),
            },
        };

        if raw_cmd.is_none() {
            return Ok(None);
        }

        let raw_cmd = raw_cmd.unwrap();

        // 如果命令是空字符串，也返回 None
        if raw_cmd.is_empty() {
            return Ok(None);
        }

        let mut command = Command::new("bash");
        command.current_dir(self.src_work_dir().unwrap());

        // 设置参数
        command.arg("-c");
        command.arg(raw_cmd);

        // 设置环境变量
        let env_list = ENV_LIST.read().unwrap();
        for (key, value) in env_list.envs.iter() {
            // if key.starts_with("DADK") {
            //     debug!("DADK env found: {}={}", key, value.value);
            // }
            command.env(key, value.value.clone());
        }
        drop(env_list);
        for (key, value) in self.local_envs.envs.iter() {
            debug!("Local env found: {}={}", key, value.value);
            command.env(key, value.value.clone());
        }

        return Ok(Some(command));
    }

    /// # 准备工作线程本地环境变量
    fn prepare_local_env(&mut self) -> Result<(), ExecutorError> {
        let binding = self.entity.task();
        let task_envs: Option<&Vec<TaskEnv>> = binding.envs.as_ref();

        if let Some(task_envs) = task_envs {
            for tv in task_envs.iter() {
                self.local_envs
                    .add(EnvVar::new(tv.key().to_string(), tv.value().to_string()));
            }
        }

        // 添加`DADK_CURRENT_BUILD_DIR`环境变量，便于构建脚本把构建结果拷贝到这里
        self.local_envs.add(EnvVar::new(
            "DADK_CURRENT_BUILD_DIR".to_string(),
            self.build_dir.path.to_str().unwrap().to_string(),
        ));

        return Ok(());
    }

    fn prepare_input(&self) -> Result<(), ExecutorError> {
        // 拉取源文件
        let task = self.entity.task();
        match &task.task_type {
            TaskType::BuildFromSource(cs) => {
                if self.source_dir.is_none() {
                    return Ok(());
                }
                let source_dir = self.source_dir.as_ref().unwrap();
                match cs {
                    CodeSource::Git(git) => {
                        git.prepare(source_dir)
                            .map_err(|e| ExecutorError::PrepareEnvError(e))?;
                    }
                    // 本地源文件，不需要拉取
                    CodeSource::Local(_) => return Ok(()),
                    // 在线压缩包，需要下载
                    CodeSource::Archive(archive) => {
                        archive
                            .download_unzip(source_dir)
                            .map_err(|e| ExecutorError::PrepareEnvError(e))?;
                    }
                }
            }
            TaskType::InstallFromPrebuilt(pb) => {
                match pb {
                    // 本地源文件，不需要拉取
                    PrebuiltSource::Local(local_source) => {
                        let local_path = local_source.path();
                        let target_path = &self.build_dir.path;
                        FileUtils::copy_dir_all(&local_path, &target_path)
                            .map_err(|e| ExecutorError::TaskFailed(e))?; // let mut cmd = "cp -r ".to_string();
                        return Ok(());
                    }
                    // 在线压缩包，需要下载
                    PrebuiltSource::Archive(archive) => {
                        archive
                            .download_unzip(&self.build_dir)
                            .map_err(|e| ExecutorError::PrepareEnvError(e))?;
                    }
                }
            }
        }

        return Ok(());
    }

    fn run_command(&self, mut command: Command) -> Result<(), ExecutorError> {
        let mut child = command
            .stdin(Stdio::inherit())
            .spawn()
            .map_err(|e| ExecutorError::IoError(e.to_string()))?;

        // 等待子进程结束
        let r = child
            .wait()
            .map_err(|e| ExecutorError::IoError(e.to_string()));
        debug!("Command finished: {:?}", r);
        if r.is_ok() {
            let r = r.unwrap();
            if r.success() {
                return Ok(());
            } else {
                // 执行失败，获取最后100行stderr输出
                let errmsg = format!(
                    "Task {} failed, exit code = {}",
                    self.entity.task().name_version(),
                    r.code().unwrap()
                );
                error!("{errmsg}");
                let command_opt = command.output();
                if command_opt.is_err() {
                    return Err(ExecutorError::TaskFailed(
                        "Failed to get command output".to_string(),
                    ));
                }
                let command_opt = command_opt.unwrap();
                let command_output = String::from_utf8_lossy(&command_opt.stderr);
                let mut last_100_outputs = command_output
                    .lines()
                    .rev()
                    .take(100)
                    .collect::<Vec<&str>>();
                last_100_outputs.reverse();
                error!("Last 100 lines msg of stderr:");
                for line in last_100_outputs {
                    error!("{}", line);
                }
                return Err(ExecutorError::TaskFailed(errmsg));
            }
        } else {
            let errmsg = format!(
                "Task {} failed, msg = {:?}",
                self.entity.task().name_version(),
                r.err().unwrap()
            );
            error!("{errmsg}");
            return Err(ExecutorError::TaskFailed(errmsg));
        }
    }
}

#[derive(Debug, Clone)]
pub struct EnvMap {
    pub envs: BTreeMap<String, EnvVar>,
}

impl EnvMap {
    pub fn new() -> Self {
        Self {
            envs: BTreeMap::new(),
        }
    }

    pub fn add(&mut self, env: EnvVar) {
        self.envs.insert(env.key.clone(), env);
    }

    #[allow(dead_code)]
    pub fn get(&self, key: &str) -> Option<&EnvVar> {
        self.envs.get(key)
    }

    pub fn add_vars(&mut self, vars: Vars) {
        for (key, value) in vars {
            self.add(EnvVar::new(key, value));
        }
    }
}

/// # 环境变量
#[derive(Debug, PartialEq, PartialOrd, Eq, Ord, Clone)]
pub struct EnvVar {
    pub key: String,
    pub value: String,
}

impl EnvVar {
    pub fn new(key: String, value: String) -> Self {
        Self { key, value }
    }
}

/// # 任务执行器错误枚举
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum ExecutorError {
    /// 准备执行环境错误
    PrepareEnvError(String),
    IoError(String),
    /// 构建执行错误
    TaskFailed(String),
    /// 安装错误
    InstallError(String),
    /// 清理错误
    CleanError(String),
}

/// # 准备全局环境变量
pub fn prepare_env(
    sched_entities: &SchedEntities,
    execute_ctx: &Arc<DadkUserExecuteContext>,
) -> Result<(), ExecutorError> {
    info!("Preparing environment variables...");
    let env_list = create_global_env_list(sched_entities, execute_ctx)?;
    // 写入全局环境变量列表
    let mut global_env_list = ENV_LIST.write().unwrap();
    *global_env_list = env_list;
    return Ok(());
}

/// # 创建全局环境变量列表
fn create_global_env_list(
    sched_entities: &SchedEntities,
    execute_ctx: &Arc<DadkUserExecuteContext>,
) -> Result<EnvMap, ExecutorError> {
    let mut env_list = EnvMap::new();
    let envs: Vars = std::env::vars();
    env_list.add_vars(envs);

    // 为每个任务创建特定的环境变量
    for entity in sched_entities.entities().iter() {
        // 导出任务的构建目录环境变量
        let build_dir = CacheDir::build_dir(entity.clone())?;

        let build_dir_key = CacheDir::build_dir_env_key(&entity)?;
        env_list.add(EnvVar::new(
            build_dir_key,
            build_dir.to_str().unwrap().to_string(),
        ));

        // 如果需要源码缓存目录，则导出
        if CacheDir::need_source_cache(entity) {
            let source_dir = CacheDir::source_dir(entity.clone())?;
            let source_dir_key = CacheDir::source_dir_env_key(&entity)?;
            env_list.add(EnvVar::new(
                source_dir_key,
                source_dir.to_str().unwrap().to_string(),
            ));
        }
    }

    // 创建ARCH环境变量
    let target_arch = execute_ctx.target_arch();
    env_list.add(EnvVar::new("ARCH".to_string(), (*target_arch).into()));

    return Ok(env_list);
}

/// # 获取文件最后的更新时间
///
/// ## 参数
/// * `path` - 文件路径
/// * `last_modified` - 最后的更新时间
/// * `build_time` - 构建时间
fn last_modified_time(
    path: &PathBuf,
    build_time: &DateTime<Utc>,
) -> Result<DateTime<Utc>, ExecutorError> {
    let mut queue = VecDeque::new();
    queue.push_back(path.clone());

    let mut last_modified = DateTime::<Utc>::from(SystemTime::UNIX_EPOCH);

    while let Some(current_path) = queue.pop_front() {
        let metadata = current_path
            .metadata()
            .map_err(|e| ExecutorError::InstallError(e.to_string()))?;

        if metadata.is_dir() {
            for r in std::fs::read_dir(&current_path).unwrap() {
                if let Ok(entry) = r {
                    // 忽略编译产物目录
                    if entry.file_name() == "target" {
                        continue;
                    }

                    let entry_path = entry.path();
                    let entry_metadata = entry.metadata().unwrap();
                    // 比较文件的修改时间和last_modified，取最大值
                    let file_modified = DateTime::<Utc>::from(entry_metadata.modified().unwrap());
                    last_modified = std::cmp::max(last_modified, file_modified);

                    // 如果其中某一个文件的修改时间在build_time之后，则直接返回，不用继续搜索
                    if last_modified > *build_time {
                        return Ok(last_modified);
                    }

                    if entry_metadata.is_dir() {
                        // 如果是子目录，则将其加入队列
                        queue.push_back(entry_path);
                    }
                }
            }
        } else {
            // 如果是文件，直接比较修改时间
            let file_modified = DateTime::<Utc>::from(metadata.modified().unwrap());
            last_modified = std::cmp::max(last_modified, file_modified);

            // 如果其中某一个文件的修改时间在build_time之后，则直接返回，不用继续递归
            if last_modified > *build_time {
                return Ok(last_modified);
            }
        }
    }

    if last_modified == DateTime::<Utc>::from(SystemTime::UNIX_EPOCH) {
        return Err(ExecutorError::InstallError(format!(
            "Failed to get last modified time for path: {}",
            path.display()
        )));
    }
    Ok(last_modified)
}
