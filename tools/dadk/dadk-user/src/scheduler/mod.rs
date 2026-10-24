use std::{
    collections::{BTreeMap, HashMap},
    fmt::Debug,
    path::PathBuf,
    process::exit,
    sync::{
        atomic::{AtomicI32, Ordering},
        Arc, Mutex, RwLock,
    },
    thread::ThreadId,
};

use chrono::{DateTime, Utc};
use log::{error, info};

use crate::{
    context::{Action, DadkUserExecuteContext},
    executor::Executor,
    parser::task::DADKTask,
};
use dadk_config::user::UserCleanLevel;

use self::task_deque::TASK_DEQUE;

pub mod task_deque;
#[cfg(test)]
mod tests;

lazy_static! {
    // 线程id与任务实体id映射表
    pub static ref TID_EID: Mutex<HashMap<ThreadId,i32>> = Mutex::new(HashMap::new());
}

/// # 调度实体内部结构
#[derive(Debug, Clone)]
pub struct InnerEntity {
    /// 任务ID
    id: i32,
    file_path: PathBuf,
    /// 任务
    task: DADKTask,
    /// 入度
    indegree: usize,
    /// 子节点
    children: Vec<Arc<SchedEntity>>,
}

/// # 调度实体
#[derive(Debug)]
pub struct SchedEntity {
    inner: Mutex<InnerEntity>,
}

impl PartialEq for SchedEntity {
    fn eq(&self, other: &Self) -> bool {
        self.inner.lock().unwrap().id == other.inner.lock().unwrap().id
    }
}

impl SchedEntity {
    #[allow(dead_code)]
    pub fn id(&self) -> i32 {
        self.inner.lock().unwrap().id
    }

    #[allow(dead_code)]
    pub fn file_path(&self) -> PathBuf {
        self.inner.lock().unwrap().file_path.clone()
    }

    #[allow(dead_code)]
    pub fn task(&self) -> DADKTask {
        self.inner.lock().unwrap().task.clone()
    }

    pub fn config_file_timestamp(&self) -> Option<DateTime<Utc>> {
        let meta = self.file_path().metadata().ok()?;
        meta.modified().map(|t| DateTime::<Utc>::from(t)).ok()
    }

    /// 入度加1
    pub fn add_indegree(&self) {
        self.inner.lock().unwrap().indegree += 1;
    }

    /// 入度减1
    pub fn sub_indegree(&self) -> usize {
        self.inner.lock().unwrap().indegree -= 1;
        return self.inner.lock().unwrap().indegree;
    }

    /// 增加子节点
    pub fn add_child(&self, entity: Arc<SchedEntity>) {
        self.inner.lock().unwrap().children.push(entity);
    }

    /// 获取入度
    pub fn indegree(&self) -> usize {
        self.inner.lock().unwrap().indegree
    }

    /// 当前任务完成后，所有子节点入度减1
    ///
    /// ## 参数
    ///
    /// 无
    ///
    /// ## 返回值
    ///
    /// 所有入度为0的子节点集合
    pub fn sub_children_indegree(&self) -> Vec<Arc<SchedEntity>> {
        let mut zero_child = Vec::new();
        let children = &self.inner.lock().unwrap().children;
        for child in children.iter() {
            if child.sub_indegree() == 0 {
                zero_child.push(child.clone());
            }
        }
        return zero_child;
    }
}

/// # 调度实体列表
///
/// 用于存储所有的调度实体
#[derive(Debug)]
pub struct SchedEntities {
    /// 任务ID到调度实体的映射
    id2entity: RwLock<BTreeMap<i32, Arc<SchedEntity>>>,
}

impl SchedEntities {
    pub fn new() -> Self {
        Self {
            id2entity: RwLock::new(BTreeMap::new()),
        }
    }

    pub fn add(&mut self, entity: Arc<SchedEntity>) {
        self.id2entity
            .write()
            .unwrap()
            .insert(entity.id(), entity.clone());
    }

    #[allow(dead_code)]
    pub fn get(&self, id: i32) -> Option<Arc<SchedEntity>> {
        self.id2entity.read().unwrap().get(&id).cloned()
    }

    pub fn get_by_name_version(&self, name: &str, version: &str) -> Option<Arc<SchedEntity>> {
        for e in self.id2entity.read().unwrap().iter() {
            if e.1.task().name_version_env() == DADKTask::name_version_uppercase(name, version) {
                return Some(e.1.clone());
            }
        }
        return None;
    }

    pub fn entities(&self) -> Vec<Arc<SchedEntity>> {
        let mut v = Vec::new();
        for e in self.id2entity.read().unwrap().iter() {
            v.push(e.1.clone());
        }
        return v;
    }

    pub fn id2entity(&self) -> BTreeMap<i32, Arc<SchedEntity>> {
        self.id2entity.read().unwrap().clone()
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.id2entity.read().unwrap().len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.id2entity.read().unwrap().is_empty()
    }

    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.id2entity.write().unwrap().clear();
    }

    pub fn topo_sort(&self) -> Vec<Arc<SchedEntity>> {
        let mut result = Vec::new();
        let mut visited = BTreeMap::new();
        let btree = self.id2entity.write().unwrap().clone();
        for entity in btree.iter() {
            if !visited.contains_key(entity.0) {
                let r = self.dfs(entity.1, &mut visited, &mut result);
                if r.is_err() {
                    let err = r.unwrap_err();
                    error!("{}", err.display());
                    println!("Please fix the errors above and try again.");
                    std::process::exit(1);
                }
            }
        }
        return result;
    }

    fn dfs(
        &self,
        entity: &Arc<SchedEntity>,
        visited: &mut BTreeMap<i32, bool>,
        result: &mut Vec<Arc<SchedEntity>>,
    ) -> Result<(), DependencyCycleError> {
        visited.insert(entity.id(), false);
        for dep in entity.task().depends.iter() {
            if let Some(dep_entity) = self.get_by_name_version(&dep.name, &dep.version) {
                let guard = self.id2entity.write().unwrap();
                let e = guard.get(&entity.id()).unwrap();
                let d = guard.get(&dep_entity.id()).unwrap();
                e.add_indegree();
                d.add_child(e.clone());
                if let Some(&false) = visited.get(&dep_entity.id()) {
                    // 输出完整环形依赖
                    let mut err = DependencyCycleError::new(dep_entity.clone());

                    err.add(entity.clone(), dep_entity);
                    return Err(err);
                }
                if !visited.contains_key(&dep_entity.id()) {
                    drop(guard);
                    let r = self.dfs(&dep_entity, visited, result);
                    if r.is_err() {
                        let mut err: DependencyCycleError = r.unwrap_err();
                        // 如果错误已经停止传播，则直接返回
                        if err.stop_propagation {
                            return Err(err);
                        }
                        // 如果当前实体是错误的起始实体，则停止传播
                        if entity == &err.head_entity {
                            err.stop_propagation();
                        }
                        err.add(entity.clone(), dep_entity);
                        return Err(err);
                    }
                }
            } else {
                error!(
                    "Dependency not found: {} -> {}",
                    entity.task().name_version(),
                    dep.name_version()
                );
                std::process::exit(1);
            }
        }
        visited.insert(entity.id(), true);
        result.push(entity.clone());
        return Ok(());
    }
}

/// # 任务调度器
#[derive(Debug)]
pub struct Scheduler {
    /// DragonOS sysroot在主机上的路径
    sysroot_dir: PathBuf,
    /// 要执行的操作
    action: Action,
    /// 调度实体列表
    target: SchedEntities,
    /// dadk执行的上下文
    context: Arc<DadkUserExecuteContext>,
}

pub enum SchedulerError {
    TaskError(String),
    /// 不是当前正在编译的目标架构
    InvalidTargetArch(String),
    DependencyNotFound(Arc<SchedEntity>, String),
    RunError(String),
}

impl Debug for SchedulerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TaskError(arg0) => {
                write!(f, "TaskError: {}", arg0)
            }
            SchedulerError::DependencyNotFound(current, msg) => {
                write!(
                    f,
                    "For task {}, dependency not found: {}. Please check file: {}",
                    current.task().name_version(),
                    msg,
                    current.file_path().display()
                )
            }
            SchedulerError::RunError(msg) => {
                write!(f, "RunError: {}", msg)
            }
            SchedulerError::InvalidTargetArch(msg) => {
                write!(f, "InvalidTargetArch: {}", msg)
            }
        }
    }
}

impl Scheduler {
    pub fn new(
        context: Arc<DadkUserExecuteContext>,
        dragonos_dir: PathBuf,
        action: Action,
        tasks: Vec<(PathBuf, DADKTask)>,
    ) -> Result<Self, SchedulerError> {
        let entities = SchedEntities::new();

        let mut scheduler = Scheduler {
            sysroot_dir: dragonos_dir,
            action,
            target: entities,
            context,
        };

        let r = scheduler.add_tasks(tasks);
        if r.is_err() {
            error!("Error while adding tasks: {:?}", r);
            return Err(r.err().unwrap());
        }

        return Ok(scheduler);
    }

    /// # 添加多个任务
    ///
    /// 添加任务到调度器中，如果任务已经存在，则返回错误
    pub fn add_tasks(&mut self, tasks: Vec<(PathBuf, DADKTask)>) -> Result<(), SchedulerError> {
        for task in tasks {
            let e = self.add_task(task.0, task.1);
            if e.is_err() {
                if let Err(SchedulerError::InvalidTargetArch(_)) = &e {
                    continue;
                }
                e?;
            }
        }

        return Ok(());
    }

    /// # 任务是否匹配当前目标架构
    pub fn task_arch_matched(&self, task: &DADKTask) -> bool {
        task.target_arch.contains(self.context.target_arch())
    }

    /// # 添加一个任务
    ///
    /// 添加任务到调度器中，如果任务已经存在，则返回错误
    pub fn add_task(
        &mut self,
        path: PathBuf,
        task: DADKTask,
    ) -> Result<Arc<SchedEntity>, SchedulerError> {
        if !self.task_arch_matched(&task) {
            return Err(SchedulerError::InvalidTargetArch(format!(
                "Task {} is not for target arch: {:?}",
                task.name_version(),
                self.context.target_arch()
            )));
        }

        let id: i32 = self.generate_task_id();
        let indegree: usize = 0;
        let children = Vec::new();
        let entity = Arc::new(SchedEntity {
            inner: Mutex::new(InnerEntity {
                id,
                task,
                file_path: path.clone(),
                indegree,
                children,
            }),
        });
        let name_version = (entity.task().name.clone(), entity.task().version.clone());

        if self
            .target
            .get_by_name_version(&name_version.0, &name_version.1)
            .is_some()
        {
            return Err(SchedulerError::TaskError(format!(
                "Task with name [{}] and version [{}] already exists. Config file: {}",
                name_version.0,
                name_version.1,
                path.display()
            )));
        }

        self.target.add(entity.clone());

        info!("Task added: {}", entity.task().name_version());
        return Ok(entity);
    }

    fn generate_task_id(&self) -> i32 {
        static TASK_ID: AtomicI32 = AtomicI32::new(0);
        return TASK_ID.fetch_add(1, Ordering::SeqCst);
    }

    /// # 执行调度器中的所有任务
    pub fn run(&self) -> Result<(), SchedulerError> {
        // 准备全局环境变量
        crate::executor::prepare_env(&self.target, &self.context)
            .map_err(|e| SchedulerError::RunError(format!("{:?}", e)))?;
        log::info!(
            "Global environment variables prepared, action: {:?}",
            self.action
        );
        match self.action {
            Action::Build | Action::Install => {
                self.run_with_topo_sort()?;
            }
            Action::Clean(_) => self.run_without_topo_sort()?,
        }

        return Ok(());
    }

    /// Action需要按照拓扑序执行
    ///
    /// Action::Build | Action::Install
    fn run_with_topo_sort(&self) -> Result<(), SchedulerError> {
        // 检查是否有不存在的依赖
        let r = self.check_not_exists_dependency();
        if r.is_err() {
            error!("Error while checking tasks: {:?}", r);
            return r;
        }

        // 对调度实体进行拓扑排序
        let r: Vec<Arc<SchedEntity>> = self.target.topo_sort();

        let action = self.action.clone();
        let dragonos_dir = self.sysroot_dir.clone();
        let id2entity = self.target.id2entity();
        let count = r.len();

        // 启动守护线程
        let handler = std::thread::spawn(move || {
            Self::build_install_daemon(action, dragonos_dir, id2entity, count, &r)
        });

        handler.join().expect("Could not join deamon");

        return Ok(());
    }

    /// Action不需要按照拓扑序执行
    fn run_without_topo_sort(&self) -> Result<(), SchedulerError> {
        // 启动守护线程
        let action = self.action.clone();
        let dragonos_dir = self.sysroot_dir.clone();
        let mut r = self.target.entities();
        let handler = std::thread::spawn(move || {
            Self::clean_daemon(action, dragonos_dir, &mut r);
        });

        handler.join().expect("Could not join deamon");
        return Ok(());
    }

    pub fn execute(action: Action, dragonos_dir: PathBuf, entity: Arc<SchedEntity>) {
        log::info!(
            "Starting task {} with action {:?}",
            entity.task().name_version(),
            action
        );
        let mut executor = Executor::new(entity.clone(), action.clone(), dragonos_dir.clone())
            .map_err(|e| {
                error!(
                    "Error while creating executor for task {} : {:?}",
                    entity.task().name_version(),
                    e
                );
                exit(-1);
            })
            .unwrap();

        executor
            .execute()
            .map_err(|e| {
                error!(
                    "Error while executing task {} : {:?}",
                    entity.task().name_version(),
                    e
                );
                exit(-1);
            })
            .unwrap();
    }

    /// 构建和安装DADK任务的守护线程
    ///
    /// ## 参数
    ///
    /// - `action` : 要执行的操作
    /// - `dragonos_dir` : DragonOS sysroot在主机上的路径
    /// - `id2entity` : DADK任务id与实体映射表
    /// - `count` : 当前剩余任务数
    /// - `r` : 总任务实体表
    ///
    /// ## 返回值
    ///
    /// 无
    pub fn build_install_daemon(
        action: Action,
        dragonos_dir: PathBuf,
        id2entity: BTreeMap<i32, Arc<SchedEntity>>,
        mut count: usize,
        r: &Vec<Arc<SchedEntity>>,
    ) {
        let mut guard = TASK_DEQUE.lock().unwrap();
        // 初始化0入度的任务实体
        let mut zero_entity: Vec<Arc<SchedEntity>> = Vec::new();
        for e in r.iter() {
            if e.indegree() == 0 {
                zero_entity.push(e.clone());
            }
        }

        while count > 0 {
            // 将入度为0的任务实体加入任务队列中，直至没有入度为0的任务实体 或 任务队列满了
            while !zero_entity.is_empty()
                && guard.build_install_task(
                    action.clone(),
                    dragonos_dir.clone(),
                    zero_entity.last().unwrap().clone(),
                )
            {
                zero_entity.pop();
            }

            let queue = guard.queue_mut();
            // 如果任务线程已完成，将其从任务队列中删除，并把它的子节点入度减1，如果有0入度子节点，则加入zero_entity，后续可以加入任务队列中
            queue.retain(|x| {
                if x.is_finished() {
                    count -= 1;
                    let tid = x.thread().id();
                    let eid = *TID_EID.lock().unwrap().get(&tid).unwrap();
                    let entity = id2entity.get(&eid).unwrap();
                    let zero = entity.sub_children_indegree();
                    for e in zero.iter() {
                        zero_entity.push(e.clone());
                    }
                    return false;
                }
                return true;
            })
        }
    }

    /// 清理DADK任务的守护线程
    ///
    /// ## 参数
    ///
    /// - `action` : 要执行的操作
    /// - `dragonos_dir` : DragonOS sysroot在主机上的路径
    /// - `r` : 总任务实体表
    ///
    /// ## 返回值
    ///
    /// 无
    pub fn clean_daemon(action: Action, dragonos_dir: PathBuf, r: &mut Vec<Arc<SchedEntity>>) {
        let mut guard = TASK_DEQUE.lock().unwrap();
        log::info!(
            "Starting clean daemon for {} tasks, queue length: {}",
            r.len(),
            guard.queue().len()
        );

        // 清理操作不需要拓扑排序，直接处理所有任务
        while !r.is_empty() {
            let entity = r.pop().unwrap();

            // 检查任务是否需要清理
            match action {
                // Clean(Output) 对所有任务都有效（清理构建输出目录）
                Action::Clean(UserCleanLevel::Output) => {
                    log::info!(
                        "Adding task {} to clean queue",
                        entity.task().name_version()
                    );
                    guard.clean_task(action, dragonos_dir.clone(), entity.clone());
                }
                // Clean(InSrc) 和 Clean(All) 需要有源码目录
                Action::Clean(_level @ (UserCleanLevel::InSrc | UserCleanLevel::All)) => {
                    // 判断任务是否有源码目录
                    // 1. 本地路径的任务（source_path 不为 None）
                    // 2. git 或 archive 类型的任务（会创建 source_dir）
                    let has_source_dir = entity.task().source_path().is_some()
                        || matches!(
                            entity.task().task_type,
                            crate::parser::task::TaskType::BuildFromSource(_)
                                | crate::parser::task::TaskType::InstallFromPrebuilt(
                                    crate::parser::task::PrebuiltSource::Archive(_)
                                )
                        );

                    if has_source_dir {
                        log::info!(
                            "Adding task {} to clean queue",
                            entity.task().name_version()
                        );
                        guard.clean_task(action, dragonos_dir.clone(), entity.clone());
                    } else {
                        log::info!(
                            "Skipping task {} (no source directory to clean)",
                            entity.task().name_version()
                        );
                    }
                }

                _ => {}
            }
        }

        // 等待所有清理任务完成
        while !guard.queue().is_empty() {
            guard.queue_mut().retain(|x| !x.is_finished());
            if !guard.queue().is_empty() {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }

    /// # 检查是否有不存在的依赖
    ///
    /// 如果某个任务的dependency中的任务不存在，则返回错误
    fn check_not_exists_dependency(&self) -> Result<(), SchedulerError> {
        for entity in self.target.entities().iter() {
            for dependency in entity.task().depends.iter() {
                let name_version = (dependency.name.clone(), dependency.version.clone());
                if !self
                    .target
                    .get_by_name_version(&name_version.0, &name_version.1)
                    .is_some()
                {
                    return Err(SchedulerError::DependencyNotFound(
                        entity.clone(),
                        format!("name:{}, version:{}", name_version.0, name_version.1,),
                    ));
                }
            }
        }

        return Ok(());
    }
}

/// # 环形依赖错误路径
///
/// 本结构体用于在回溯过程中记录环形依赖的路径。
///
/// 例如，假设有如下依赖关系：
///
/// ```text
/// A -> B -> C -> D -> A
/// ```
///
/// 则在DFS回溯过程中，会依次记录如下路径：
///
/// ```text
/// D -> A
/// C -> D
/// B -> C
/// A -> B
pub struct DependencyCycleError {
    /// # 起始实体
    /// 本错误的起始实体，即环形依赖的起点
    head_entity: Arc<SchedEntity>,
    /// 是否停止传播
    stop_propagation: bool,
    /// 依赖关系
    dependencies: Vec<(Arc<SchedEntity>, Arc<SchedEntity>)>,
}

impl DependencyCycleError {
    pub fn new(head_entity: Arc<SchedEntity>) -> Self {
        Self {
            head_entity,
            stop_propagation: false,
            dependencies: Vec::new(),
        }
    }

    pub fn add(&mut self, current: Arc<SchedEntity>, dependency: Arc<SchedEntity>) {
        self.dependencies.push((current, dependency));
    }

    pub fn stop_propagation(&mut self) {
        self.stop_propagation = true;
    }

    #[allow(dead_code)]
    pub fn dependencies(&self) -> &Vec<(Arc<SchedEntity>, Arc<SchedEntity>)> {
        &self.dependencies
    }

    pub fn display(&self) -> String {
        let mut tmp = self.dependencies.clone();
        tmp.reverse();

        let mut ret = format!("Dependency cycle detected: \nStart ->\n");
        for (current, dep) in tmp.iter() {
            ret.push_str(&format!(
                "->\t{} ({})\t--depends-->\t{} ({})\n",
                current.task().name_version(),
                current.file_path().display(),
                dep.task().name_version(),
                dep.file_path().display()
            ));
        }
        ret.push_str("-> End");
        return ret;
    }
}
