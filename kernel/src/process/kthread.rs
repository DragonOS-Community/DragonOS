use core::{hint::spin_loop, sync::atomic::Ordering};

use alloc::{
    boxed::Box,
    collections::LinkedList,
    string::{String, ToString},
    sync::Arc,
};
use atomic_enum::atomic_enum;

use crate::{
    arch::{sched::sched, CurrentIrqArch},
    exception::InterruptArch,
    kinfo,
    libs::{once::Once, spinlock::SpinLock},
    process::{ProcessManager, ProcessState},
    syscall::SystemError, kdebug,
};

use super::{
    fork::CloneFlags, init::initial_kernel_thread, Pid, ProcessControlBlock, ProcessFlags,
};

/// 内核线程的创建任务列表
static KTHREAD_CREATE_LIST: SpinLock<LinkedList<Arc<KernelThreadCreateInfo>>> =
    SpinLock::new(LinkedList::new());

static mut KTHREAD_DAEMON_PCB: Option<Arc<ProcessControlBlock>> = None;

#[derive(Debug)]
pub enum WorkerPrivate {
    KernelThread(KernelThreadPcbPrivate),
}

impl WorkerPrivate {
    pub fn kernel_thread(&self) -> Option<&KernelThreadPcbPrivate> {
        match self {
            Self::KernelThread(x) => Some(x),
        }
    }

    pub fn kernel_thread_mut(&mut self) -> Option<&mut KernelThreadPcbPrivate> {
        match self {
            Self::KernelThread(x) => Some(x),
        }
    }
}

bitflags! {
    pub struct KernelThreadFlags: u32 {
        const IS_PER_CPU = 1 << 0;
        const SHOULD_STOP = 1 << 1;
        const SHOULD_PARK = 1 << 2;
    }
}

#[derive(Debug)]
pub struct KernelThreadPcbPrivate {
    flags: KernelThreadFlags,
}

impl KernelThreadPcbPrivate {
    pub fn new() -> Self {
        Self {
            flags: KernelThreadFlags::empty(),
        }
    }

    pub fn flags(&self) -> &KernelThreadFlags {
        &self.flags
    }

    pub fn flags_mut(&mut self) -> &mut KernelThreadFlags {
        &mut self.flags
    }
}

/// 内核线程的闭包，参数必须与闭包的参数一致，返回值必须是i32
///
/// 元组的第一个元素是闭包，第二个元素是闭包的参数对象
///
/// 对于非原始类型的参数，需要使用Box包装
pub enum KernelThreadClosure {
    UsizeClosure((Box<dyn Fn(usize) -> i32 + Send + Sync>, usize)),
    EmptyClosure((Box<dyn Fn() -> i32 + Send + Sync>, ())),
    // 添加其他类型入参的闭包，返回值必须是i32
}

impl KernelThreadClosure {
    pub fn run(self) -> i32 {
        match self {
            Self::UsizeClosure((func, arg)) => func(arg),
            Self::EmptyClosure((func, arg)) => func(),
        }
    }
}

pub struct KernelThreadCreateInfo {
    /// 内核线程的入口函数、传入参数
    closure: SpinLock<Option<Box<KernelThreadClosure>>>,
    /// 内核线程的名字
    name: String,
    /// 是否已经完成创建 todo:使用comletion机制优化这里
    created: AtomicKernelThreadCreateStatus,
    result_pcb: SpinLock<Option<Arc<ProcessControlBlock>>>,
}

#[atomic_enum]
#[derive(PartialEq)]
pub enum KernelThreadCreateStatus {
    Created,
    NotCreated,
    ErrorOccured,
}

impl KernelThreadCreateInfo {
    pub fn new(func: KernelThreadClosure, name: String) -> Arc<Self> {
        Arc::new(Self {
            closure: SpinLock::new(Some(Box::new(func))),
            name,
            created: AtomicKernelThreadCreateStatus::new(KernelThreadCreateStatus::NotCreated),
            result_pcb: SpinLock::new(None),
        })
    }

    /// 创建者调用这函数，等待创建完成后，获取创建结果
    ///
    /// ## 返回值
    ///
    /// - Some(Arc<ProcessControlBlock>) 创建成功，返回新创建的内核线程的PCB
    /// - None 创建失败
    pub fn poll_result(&self) -> Option<Arc<ProcessControlBlock>> {
        loop {
            match self.created.load(Ordering::SeqCst) {
                KernelThreadCreateStatus::Created => {
                    return self.result_pcb.lock().take();
                }
                KernelThreadCreateStatus::NotCreated => {
                    spin_loop();
                }
                KernelThreadCreateStatus::ErrorOccured => {
                    return None;
                }
            }
        }
    }

    pub fn take_closure(&self) -> Option<Box<KernelThreadClosure>> {
        return self.closure.lock().take();
    }
}

pub struct KernelThreadMechanism;

impl KernelThreadMechanism {
    pub fn init_stage1() {
        kinfo!("Initializing kernel thread mechanism stage1...");

        // 初始化第一个内核线程

        let create_info = KernelThreadCreateInfo::new(
            KernelThreadClosure::EmptyClosure((Box::new(initial_kernel_thread), ())),
            "init".to_string(),
        );
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        // 由于当前是pid=0的idle进程,而__inner_create要求当前是kthread,所以先临时设置为kthread
        ProcessManager::current_pcb()
            .flags
            .lock()
            .insert(ProcessFlags::KTHREAD);

        KernelThreadMechanism::__inner_create(
            &create_info,
            CloneFlags::CLONE_VM | CloneFlags::CLONE_SIGNAL,
        )
        .unwrap_or_else(|e| panic!("Failed to create initial kernel thread, error: {:?}", e));

        ProcessManager::current_pcb()
            .flags
            .lock()
            .remove(ProcessFlags::KTHREAD);
        drop(irq_guard);
        kinfo!("Initializing kernel thread mechanism stage1 complete");
    }

    pub fn init_stage2() {
        assert!(ProcessManager::current_pcb()
            .flags()
            .contains(ProcessFlags::KTHREAD));
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            kinfo!("Initializing kernel thread mechanism stage2...");
            // 初始化kthreadd
            let closure = KernelThreadClosure::EmptyClosure((Box::new(Self::kthread_daemon), ()));
            let info = KernelThreadCreateInfo::new(closure, "kthreadd".to_string());
            let kthreadd_pid: Pid =
                Self::__inner_create(&info, CloneFlags::CLONE_FS | CloneFlags::CLONE_SIGNAL)
                    .expect("Failed to create kthread daemon");

            let pcb = ProcessManager::find(kthreadd_pid).unwrap();
            unsafe {
                KTHREAD_DAEMON_PCB.replace(pcb);
            }
            kinfo!("Initializing kernel thread mechanism stage2 complete");
        });
    }

    /// 创建一个新的内核线程
    ///
    /// ## 参数
    ///
    /// - func: 内核线程的入口函数、传入参数
    /// - name: 内核线程的名字
    ///
    /// ## 返回值
    ///
    /// - Some(Arc<ProcessControlBlock>) 创建成功，返回新创建的内核线程的PCB
    pub fn create(func: KernelThreadClosure, name: String) -> Option<Arc<ProcessControlBlock>> {
        let info = KernelThreadCreateInfo::new(func, name);
        while unsafe { KTHREAD_DAEMON_PCB.is_none() } {
            // 等待kthreadd启动
            spin_loop()
        }
        KTHREAD_CREATE_LIST.lock().push_back(info.clone());
        ProcessManager::wakeup(unsafe { KTHREAD_DAEMON_PCB.as_ref().unwrap() })
            .expect("Failed to wakeup kthread daemon");
        return info.poll_result();
    }

    /// 停止一个内核线程
    ///
    /// 如果目标内核线程的数据检查失败，会panic
    ///
    /// ## 返回值
    ///
    /// - Ok(i32) 目标内核线程的退出码
    pub fn stop(pcb: &Arc<ProcessControlBlock>) -> Result<usize, SystemError> {
        if !pcb.flags().contains(ProcessFlags::KTHREAD) {
            panic!("Cannt stop a non-kthread process");
        }

        let mut worker_private = pcb.worker_private();
        assert!(
            worker_private.is_some(),
            "kthread stop: worker_private is none, pid: {:?}",
            pcb.pid()
        );
        worker_private
            .as_mut()
            .unwrap()
            .kernel_thread_mut()
            .expect("Error type of worker private")
            .flags
            .insert(KernelThreadFlags::SHOULD_STOP);

        drop(worker_private);

        ProcessManager::wakeup(pcb).ok();

        // 忙等目标内核线程退出
        // todo: 使用completion机制优化这里
        loop {
            if let ProcessState::Exited(code) = pcb.sched_info().state() {
                return Ok(code);
            }
            spin_loop();
        }
    }

    /// 判断一个内核线程是否应当停止
    ///
    /// ## 参数
    ///
    /// - pcb: 目标内核线程的PCB
    ///
    /// ## 返回值
    ///
    /// - bool 是否应当停止. true表示应当停止，false表示不应当停止. 如果目标进程不是内核线程，返回false
    ///
    /// ## Panic
    ///
    /// 如果目标内核线程的数据检查失败，会panic
    pub fn should_stop(pcb: &Arc<ProcessControlBlock>) -> bool {
        if !pcb.flags().contains(ProcessFlags::KTHREAD) {
            return false;
        }

        let worker_private = pcb.worker_private();
        assert!(
            worker_private.is_some(),
            "kthread should_stop: worker_private is none, pid: {:?}",
            pcb.pid()
        );
        return worker_private
            .as_ref()
            .unwrap()
            .kernel_thread()
            .expect("Error type of worker private")
            .flags
            .contains(KernelThreadFlags::SHOULD_STOP);
    }

    /// A daemon thread which creates other kernel threads
    fn kthread_daemon() -> i32 {
        let current_pcb = ProcessManager::current_pcb();
        kdebug!("kthread_daemon: pid: {:?}", current_pcb.pid());
        {
            // 初始化worker_private
            let mut worker_private_guard = current_pcb.worker_private();
            let worker_private = WorkerPrivate::KernelThread(KernelThreadPcbPrivate::new());
            *worker_private_guard = Some(worker_private);
        }
        // 设置为kthread
        current_pcb.flags().insert(ProcessFlags::KTHREAD);
        drop(current_pcb);

        loop {
            let mut list = KTHREAD_CREATE_LIST.lock();
            while let Some(info) = list.pop_front() {
                drop(list);

                // create a new kernel thread
                let result: Result<Pid, SystemError> =
                    Self::__inner_create(&info, CloneFlags::CLONE_FS | CloneFlags::CLONE_SIGNAL);

                if let Ok(pid) = result {
                    // 创建成功
                    info.created
                        .store(KernelThreadCreateStatus::Created, Ordering::SeqCst);
                    let pcb = ProcessManager::find(pid).unwrap();
                    info.result_pcb.lock().replace(pcb);
                } else {
                    // 创建失败
                    info.created
                        .store(KernelThreadCreateStatus::ErrorOccured, Ordering::SeqCst);
                }
                list = KTHREAD_CREATE_LIST.lock();
            }
            drop(list);

            let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
            ProcessManager::mark_sleep(true).ok();
            drop(irq_guard);
            sched();
        }
    }
}

/// 内核线程启动的第二阶段
///
/// 该函数只能被`kernel_thread_bootstrap_stage1`调用（jmp到该函数）
///
/// ## 参数
///
/// - ptr: 传入的参数，是一个指向`Box<KernelThreadClosure>`的指针
pub unsafe extern "C" fn kernel_thread_bootstrap_stage2(ptr: *mut KernelThreadClosure) -> ! {
    let closure = Box::from_raw(ptr);
    let retval = closure.run() as usize;
    ProcessManager::exit(retval);
}

pub fn kthread_init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        KernelThreadMechanism::init_stage1();
    });
}