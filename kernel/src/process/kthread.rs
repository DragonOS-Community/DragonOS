use core::{
    hint::spin_loop,
    sync::atomic::{AtomicBool, Ordering},
};

use alloc::{
    boxed::Box,
    collections::LinkedList,
    string::{String, ToString},
    sync::{Arc, Weak},
};
use atomic_enum::atomic_enum;
use system_error::SystemError;

use crate::{
    arch::{sched::sched, CurrentIrqArch},
    exception::{irqdesc::IrqAction, InterruptArch},
    init::initial_kthread::initial_kernel_thread,
    kdebug, kinfo,
    libs::{once::Once, spinlock::SpinLock},
    process::{ProcessManager, ProcessState},
};

use super::{fork::CloneFlags, Pid, ProcessControlBlock, ProcessFlags};

/// 内核线程的创建任务列表
static KTHREAD_CREATE_LIST: SpinLock<LinkedList<Arc<KernelThreadCreateInfo>>> =
    SpinLock::new(LinkedList::new());

static mut KTHREAD_DAEMON_PCB: Option<Arc<ProcessControlBlock>> = None;

#[derive(Debug)]
pub enum WorkerPrivate {
    KernelThread(KernelThreadPcbPrivate),
}

#[allow(dead_code)]
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

#[allow(dead_code)]
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
#[allow(dead_code)]
pub enum KernelThreadClosure {
    UsizeClosure((Box<dyn Fn(usize) -> i32 + Send + Sync>, usize)),
    StaticUsizeClosure((&'static fn(usize) -> i32, usize)),
    EmptyClosure((Box<dyn Fn() -> i32 + Send + Sync>, ())),
    StaticEmptyClosure((&'static fn() -> i32, ())),
    IrqThread(
        (
            &'static dyn Fn(Arc<IrqAction>) -> Result<(), SystemError>,
            Arc<IrqAction>,
        ),
    ),
    // 添加其他类型入参的闭包，返回值必须是i32
}

unsafe impl Send for KernelThreadClosure {}
unsafe impl Sync for KernelThreadClosure {}

impl KernelThreadClosure {
    pub fn run(self) -> i32 {
        match self {
            Self::UsizeClosure((func, arg)) => func(arg),
            Self::EmptyClosure((func, _arg)) => func(),
            Self::StaticUsizeClosure((func, arg)) => func(arg),
            Self::StaticEmptyClosure((func, _arg)) => func(),
            Self::IrqThread((func, arg)) => {
                func(arg).map(|_| 0).unwrap_or_else(|e| e.to_posix_errno())
            }
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
    /// 不安全的Arc引用计数，当内核线程创建失败时，需要减少这个计数
    has_unsafe_arc_instance: AtomicBool,
    self_ref: Weak<Self>,
    /// 如果该值为true在进入bootstrap stage2之后，就会进入睡眠状态
    to_mark_sleep: AtomicBool,
}

#[atomic_enum]
#[derive(PartialEq)]
pub enum KernelThreadCreateStatus {
    Created,
    NotCreated,
    ErrorOccured,
}

#[allow(dead_code)]
impl KernelThreadCreateInfo {
    pub fn new(func: KernelThreadClosure, name: String) -> Arc<Self> {
        let result = Arc::new(Self {
            closure: SpinLock::new(Some(Box::new(func))),
            name,
            created: AtomicKernelThreadCreateStatus::new(KernelThreadCreateStatus::NotCreated),
            result_pcb: SpinLock::new(None),
            has_unsafe_arc_instance: AtomicBool::new(false),
            self_ref: Weak::new(),
            to_mark_sleep: AtomicBool::new(true),
        });
        let tmp = result.clone();
        unsafe {
            let tmp = Arc::into_raw(tmp) as *mut Self;
            (*tmp).self_ref = Arc::downgrade(&result);
            Arc::from_raw(tmp);
        }

        return result;
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
                    // 创建失败，减少不安全的Arc引用计数
                    let to_delete = self.has_unsafe_arc_instance.swap(false, Ordering::SeqCst);
                    if to_delete {
                        let self_ref = self.self_ref.upgrade().unwrap();
                        unsafe { Arc::decrement_strong_count(&self_ref) };
                    }
                    return None;
                }
            }
        }
    }

    pub fn take_closure(&self) -> Option<Box<KernelThreadClosure>> {
        return self.closure.lock().take();
    }

    pub fn name(&self) -> &String {
        &self.name
    }

    pub unsafe fn set_create_ok(&self, pcb: Arc<ProcessControlBlock>) {
        // todo: 使用completion机制优化这里
        self.result_pcb.lock().replace(pcb);
        self.created
            .store(KernelThreadCreateStatus::Created, Ordering::SeqCst);
    }

    /// 生成一个不安全的Arc指针（用于创建内核线程时传递参数）
    pub fn generate_unsafe_arc_ptr(self: Arc<Self>) -> *const Self {
        assert!(
            self.has_unsafe_arc_instance
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok(),
            "Cannot generate unsafe arc ptr when there is already one."
        );
        let ptr = Arc::into_raw(self);
        return ptr;
    }

    pub unsafe fn parse_unsafe_arc_ptr(ptr: *const Self) -> Arc<Self> {
        let arc = Arc::from_raw(ptr);
        assert!(
            arc.has_unsafe_arc_instance
                .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok(),
            "Cannot parse unsafe arc ptr when there is no one."
        );
        assert!(Arc::strong_count(&arc) > 0);
        return arc;
    }

    /// 设置是否在进入bootstrap stage2之后，就进入睡眠状态
    ///
    /// ## 参数
    ///
    /// - to_mark_sleep: 是否在进入bootstrap stage2之后，就进入睡眠状态
    ///
    /// ## 返回值
    /// 如果已经创建完成，返回EINVAL
    pub fn set_to_mark_sleep(&self, to_mark_sleep: bool) -> Result<(), SystemError> {
        let result_guard = self.result_pcb.lock();
        if result_guard.is_some() {
            // 已经创建完成，不需要设置
            return Err(SystemError::EINVAL);
        }
        self.to_mark_sleep.store(to_mark_sleep, Ordering::SeqCst);
        return Ok(());
    }

    pub fn to_mark_sleep(&self) -> bool {
        self.to_mark_sleep.load(Ordering::SeqCst)
    }
}

pub struct KernelThreadMechanism;

impl KernelThreadMechanism {
    pub fn init_stage1() {
        assert!(ProcessManager::current_pcb().pid() == Pid::new(0));
        kinfo!("Initializing kernel thread mechanism stage1...");

        // 初始化第一个内核线程

        let create_info = KernelThreadCreateInfo::new(
            KernelThreadClosure::EmptyClosure((Box::new(initial_kernel_thread), ())),
            "init".to_string(),
        );

        let irq_guard: crate::exception::IrqFlagsGuard =
            unsafe { CurrentIrqArch::save_and_disable_irq() };
        // 由于当前是pid=0的idle进程,而__inner_create要求当前是kthread,所以先临时设置为kthread
        ProcessManager::current_pcb()
            .flags
            .get_mut()
            .insert(ProcessFlags::KTHREAD);
        create_info
            .set_to_mark_sleep(false)
            .expect("Failed to set to_mark_sleep");

        KernelThreadMechanism::__inner_create(
            &create_info,
            CloneFlags::CLONE_VM | CloneFlags::CLONE_SIGNAL,
        )
        .unwrap_or_else(|e| panic!("Failed to create initial kernel thread, error: {:?}", e));

        ProcessManager::current_pcb()
            .flags
            .get_mut()
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
            let kthreadd_pid: Pid = Self::__inner_create(
                &info,
                CloneFlags::CLONE_VM | CloneFlags::CLONE_FS | CloneFlags::CLONE_SIGNAL,
            )
            .expect("Failed to create kthread daemon");
            let pcb = ProcessManager::find(kthreadd_pid).unwrap();
            ProcessManager::wakeup(&pcb).expect("Failed to wakeup kthread daemon");
            unsafe {
                KTHREAD_DAEMON_PCB.replace(pcb);
            }
            kinfo!("Initialize kernel thread mechanism stage2 complete");
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
    #[allow(dead_code)]
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

    /// 创建并运行一个新的内核线程
    ///
    /// ## 参数
    ///
    /// - func: 内核线程的入口函数、传入参数
    /// - name: 内核线程的名字
    ///
    /// ## 返回值
    ///
    /// - Some(Arc<ProcessControlBlock>) 创建成功，返回新创建的内核线程的PCB
    #[allow(dead_code)]
    pub fn create_and_run(
        func: KernelThreadClosure,
        name: String,
    ) -> Option<Arc<ProcessControlBlock>> {
        let pcb = Self::create(func, name)?;
        ProcessManager::wakeup(&pcb)
            .expect(format!("Failed to wakeup kthread: {:?}", pcb.pid()).as_str());
        return Some(pcb);
    }

    /// 停止一个内核线程
    ///
    /// 如果目标内核线程的数据检查失败，会panic
    ///
    /// ## 返回值
    ///
    /// - Ok(i32) 目标内核线程的退出码
    #[allow(dead_code)]
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
            if let ProcessState::Exited(code) = pcb.sched_info().inner_lock_read_irqsave().state() {
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
    #[allow(dead_code)]
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
    #[inline(never)]
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
                let result: Result<Pid, SystemError> = Self::__inner_create(
                    &info,
                    CloneFlags::CLONE_VM | CloneFlags::CLONE_FS | CloneFlags::CLONE_SIGNAL,
                );
                if result.is_err() {
                    // 创建失败
                    info.created
                        .store(KernelThreadCreateStatus::ErrorOccured, Ordering::SeqCst);
                };
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
/// - ptr: 传入的参数，是一个指向`Arc<KernelThreadCreateInfo>`的指针
pub unsafe extern "C" fn kernel_thread_bootstrap_stage2(ptr: *const KernelThreadCreateInfo) -> ! {
    let info = KernelThreadCreateInfo::parse_unsafe_arc_ptr(ptr);

    let closure: Box<KernelThreadClosure> = info.take_closure().unwrap();
    info.set_create_ok(ProcessManager::current_pcb());
    let to_mark_sleep = info.to_mark_sleep();
    drop(info);

    if to_mark_sleep {
        // 进入睡眠状态
        let irq_guard = CurrentIrqArch::save_and_disable_irq();
        ProcessManager::mark_sleep(true).expect("Failed to mark sleep");
        drop(irq_guard);
        sched();
    }

    let mut retval = SystemError::EINTR.to_posix_errno();

    if !KernelThreadMechanism::should_stop(&ProcessManager::current_pcb()) {
        retval = closure.run();
    }

    ProcessManager::exit(retval as usize);
}

/// 初始化内核线程机制
#[inline(never)]
pub fn kthread_init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        KernelThreadMechanism::init_stage1();
    });
}
