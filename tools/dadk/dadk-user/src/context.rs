use std::{
    path::PathBuf,
    process::exit,
    sync::{Arc, Mutex, Weak},
};

use dadk_config::{
    app_blocklist::AppBlocklistConfigFile, common::target_arch::TargetArch, user::UserCleanLevel,
};
use derive_builder::Builder;
use log::error;
#[cfg(test)]
use test_base::{global::BaseGlobalTestContext, test_context::TestContext};

use crate::{executor::cache::cache_root_init, scheduler::task_deque::TASK_DEQUE};

#[derive(Debug, Builder)]
#[builder(setter(into))]
pub struct DadkUserExecuteContext {
    /// DragonOS sysroot在主机上的路径
    sysroot_dir: Option<PathBuf>,
    /// DADK任务配置文件所在目录
    config_dir: Option<PathBuf>,
    /// 要执行的操作
    action: Action,
    /// 并行线程数量
    thread_num: Option<usize>,
    /// dadk缓存根目录
    cache_dir: Option<PathBuf>,

    /// 目标架构
    #[builder(default = "crate::DADKTask::default_target_arch()")]
    target_arch: TargetArch,

    /// 应用程序黑名单配置
    app_blocklist: Option<AppBlocklistConfigFile>,

    #[cfg(test)]
    base_test_context: Option<BaseGlobalTestContext>,

    #[builder(setter(skip), default = "Mutex::new(Weak::new())")]
    self_ref: Mutex<Weak<Self>>,
}

impl DadkUserExecuteContext {
    pub fn init(&self, self_arc: Arc<Self>) {
        self.set_self_ref(Arc::downgrade(&self_arc));

        // 初始化缓存目录
        let r: Result<(), crate::executor::ExecutorError> =
            cache_root_init(self.cache_dir().cloned());
        if r.is_err() {
            error!("Failed to init cache root: {:?}", r.unwrap_err());
            exit(1);
        }

        if let Some(thread) = self.thread_num() {
            TASK_DEQUE.lock().unwrap().set_thread(thread);
        }

        if self.config_dir().is_none() {
            error!("Config dir is required for action: {:?}", self.action());
            exit(1);
        }

        if self.sysroot_dir().is_none() {
            error!(
                "dragonos sysroot dir is required for action: {:?}",
                self.action()
            );
            exit(1);
        }
    }

    #[allow(dead_code)]
    pub fn self_ref(&self) -> Option<Arc<Self>> {
        self.self_ref.lock().unwrap().upgrade()
    }

    fn set_self_ref(&self, self_ref: Weak<Self>) {
        *self.self_ref.lock().unwrap() = self_ref;
    }

    pub fn target_arch(&self) -> &TargetArch {
        &self.target_arch
    }

    pub fn sysroot_dir(&self) -> Option<&PathBuf> {
        self.sysroot_dir.as_ref()
    }

    pub fn config_dir(&self) -> Option<&PathBuf> {
        self.config_dir.as_ref()
    }

    pub fn action(&self) -> &Action {
        &self.action
    }

    pub fn thread_num(&self) -> Option<usize> {
        self.thread_num
    }

    pub fn cache_dir(&self) -> Option<&PathBuf> {
        self.cache_dir.as_ref()
    }

    pub fn app_blocklist(&self) -> &Option<AppBlocklistConfigFile> {
        &self.app_blocklist
    }

    pub fn set_app_blocklist(&mut self, blocklist: AppBlocklistConfigFile) {
        self.app_blocklist = Some(blocklist);
    }
}

#[cfg(test)]
pub trait TestContextExt: TestContext {
    fn base_context(&self) -> &BaseGlobalTestContext;

    fn execute_context(&self) -> &DadkUserExecuteContext;
}

impl DadkUserExecuteContextBuilder {
    /// 用于测试的默认构建器
    #[cfg(test)]
    fn default_test_execute_context_builder(base_context: &BaseGlobalTestContext) -> Self {
        Self::default()
            .sysroot_dir(Some(base_context.fake_dragonos_sysroot()))
            .action(Action::Build)
            .thread_num(None)
            .cache_dir(Some(base_context.fake_dadk_cache_root()))
            .base_test_context(Some(base_context.clone()))
            .clone()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// 构建所有项目
    Build,
    /// 清理缓存
    Clean(UserCleanLevel),
    /// 安装到DragonOS sysroot
    Install,
}

#[cfg(test)]
pub struct DadkExecuteContextTestBuildX86_64V1 {
    context: Arc<DadkUserExecuteContext>,
}

#[cfg(test)]
impl TestContext for DadkExecuteContextTestBuildX86_64V1 {
    fn setup() -> Self {
        let base_context = BaseGlobalTestContext::setup();
        let context =
            DadkUserExecuteContextBuilder::default_test_execute_context_builder(&base_context)
                .target_arch(TargetArch::X86_64)
                .config_dir(Some(base_context.config_v1_dir()))
                .app_blocklist(None)
                .build()
                .expect("Failed to build DadkExecuteContextTestBuildX86_64V1");
        let context = Arc::new(context);
        context.init(context.clone());
        DadkExecuteContextTestBuildX86_64V1 { context }
    }
}

#[cfg(test)]
pub struct DadkExecuteContextTestBuildRiscV64V1 {
    context: Arc<DadkUserExecuteContext>,
}

#[cfg(test)]
impl TestContext for DadkExecuteContextTestBuildRiscV64V1 {
    fn setup() -> Self {
        let base_context = BaseGlobalTestContext::setup();
        let context =
            DadkUserExecuteContextBuilder::default_test_execute_context_builder(&base_context)
                .target_arch(TargetArch::RiscV64)
                .config_dir(Some(base_context.config_v1_dir()))
                .app_blocklist(None)
                .build()
                .expect("Failed to build DadkExecuteContextTestBuildRiscV64V1");
        let context = Arc::new(context);
        context.init(context.clone());
        DadkExecuteContextTestBuildRiscV64V1 { context }
    }
}

macro_rules! impl_for_test_context {
    ($context:ty) => {
        #[cfg(test)]
        impl std::ops::Deref for $context {
            type Target = DadkUserExecuteContext;

            fn deref(&self) -> &Self::Target {
                &self.context
            }
        }

        #[cfg(test)]
        impl TestContextExt for $context {
            fn base_context(&self) -> &BaseGlobalTestContext {
                self.base_test_context.as_ref().unwrap()
            }

            fn execute_context(&self) -> &DadkUserExecuteContext {
                &self.context
            }
        }
    };
}

impl_for_test_context!(DadkExecuteContextTestBuildX86_64V1);
impl_for_test_context!(DadkExecuteContextTestBuildRiscV64V1);
