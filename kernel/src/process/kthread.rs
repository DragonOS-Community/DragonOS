use core::{
    hint::spin_loop,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use alloc::{
    boxed::Box,
    collections::LinkedList,
    string::{String, ToString},
    sync::{Arc, Weak},
};
use atomic_enum::atomic_enum;
use log::info;
use system_error::SystemError;

use crate::{
    arch::CurrentIrqArch,
    exception::{irqdesc::IrqAction, InterruptArch},
    init::initial_kthread::{initial_kernel_thread, set_system_state, SystemState},
    libs::{once::Once, spinlock::SpinLock, wait_queue::WaitQueue},
    process::{ProcessManager, ProcessState},
    sched::{completion::Completion, schedule, SchedMode},
    smp::cpu::ProcessorId,
};

use super::{fork::CloneFlags, ProcessControlBlock, ProcessFlags, RawPid};

/// 内核线程的创建任务列表
static KTHREAD_CREATE_LIST: SpinLock<LinkedList<Arc<KernelThreadCreateInfo>>> =
    SpinLock::new(LinkedList::new());

/// All work that can make kthreadd useful must use this notification path.
/// The pending bit coalesces events because each daemon pass drains the full
/// create list and reaps every zombie child.
static KTHREAD_DAEMON_WAIT: WaitQueue = WaitQueue::default();
static KTHREAD_DAEMON_WORK_PENDING: AtomicBool = AtomicBool::new(false);
static KTHREAD_DAEMON_READY: AtomicBool = AtomicBool::new(false);
static KTHREAD_SELFTEST_RUNNING: AtomicBool = AtomicBool::new(false);

struct KthreadSelftestGuard;

impl Drop for KthreadSelftestGuard {
    fn drop(&mut self) {
        KTHREAD_SELFTEST_RUNNING.store(false, Ordering::Release);
    }
}

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
    result: usize,
    exited: Arc<Completion>,
}

#[allow(dead_code)]
impl KernelThreadPcbPrivate {
    pub fn new() -> Self {
        Self {
            flags: KernelThreadFlags::empty(),
            result: 0,
            exited: Arc::new(Completion::new()),
        }
    }

    pub fn flags(&self) -> &KernelThreadFlags {
        &self.flags
    }

    pub fn flags_mut(&mut self) -> &mut KernelThreadFlags {
        &mut self.flags
    }

    pub fn result(&self) -> usize {
        self.result
    }

    pub fn set_result(&mut self, result: usize) {
        self.result = result;
    }

    pub fn exited_completion(&self) -> Arc<Completion> {
        self.exited.clone()
    }
}

impl Default for KernelThreadPcbPrivate {
    fn default() -> Self {
        Self::new()
    }
}

/// 内核线程的闭包，参数必须与闭包的参数一致，返回值必须是i32
///
/// 元组的第一个元素是闭包，第二个元素是闭包的参数对象
///
/// 对于非原始类型的参数，需要使用Box包装
#[allow(dead_code)]
#[allow(clippy::type_complexity)]
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
    /// 是否已经完成创建
    created: AtomicKernelThreadCreateStatus,
    created_completion: Completion,
    result_pcb: SpinLock<Option<Arc<ProcessControlBlock>>>,
    /// 不安全的Arc引用计数，当内核线程创建失败时，需要减少这个计数
    has_unsafe_arc_instance: AtomicBool,
    self_ref: Weak<Self>,
    /// 如果该值为true在进入bootstrap stage2之后，就会进入睡眠状态
    to_mark_sleep: AtomicBool,
    flags: SpinLock<KernelThreadFlags>,
    bound_cpu: SpinLock<Option<ProcessorId>>,
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
            created_completion: Completion::new(),
            result_pcb: SpinLock::new(None),
            has_unsafe_arc_instance: AtomicBool::new(false),
            self_ref: Weak::new(),
            to_mark_sleep: AtomicBool::new(true),
            flags: SpinLock::new(KernelThreadFlags::empty()),
            bound_cpu: SpinLock::new(None),
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
        self.created_completion
            .wait_for_completion()
            .expect("kthread create completion wait failed");

        match self.created.load(Ordering::SeqCst) {
            KernelThreadCreateStatus::Created => self.result_pcb.lock().take(),
            KernelThreadCreateStatus::ErrorOccured => {
                // 创建失败，减少不安全的Arc引用计数
                let to_delete = self.has_unsafe_arc_instance.swap(false, Ordering::SeqCst);
                if to_delete {
                    let self_ref = self.self_ref.upgrade().unwrap();
                    unsafe { Arc::decrement_strong_count(&self_ref) };
                }
                None
            }
            KernelThreadCreateStatus::NotCreated => {
                panic!("kthread create completion published without a result")
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
        self.result_pcb.lock().replace(pcb);
        self.created
            .store(KernelThreadCreateStatus::Created, Ordering::SeqCst);
        self.created_completion.complete();
    }

    pub fn set_create_error(&self) {
        self.created
            .store(KernelThreadCreateStatus::ErrorOccured, Ordering::SeqCst);
        self.created_completion.complete();
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

    pub fn set_per_cpu(&self, cpu: ProcessorId) -> Result<(), SystemError> {
        let result_guard = self.result_pcb.lock();
        if result_guard.is_some() {
            return Err(SystemError::EINVAL);
        }
        drop(result_guard);

        self.flags.lock().insert(KernelThreadFlags::IS_PER_CPU);
        *self.bound_cpu.lock() = Some(cpu);
        Ok(())
    }

    pub fn setup_pcb(&self, pcb: &Arc<ProcessControlBlock>) {
        let flags = *self.flags.lock();
        if flags.is_empty() {
            return;
        }

        let mut worker_private_guard = pcb.worker_private();
        let worker_private = worker_private_guard
            .as_mut()
            .and_then(|x| x.kernel_thread_mut())
            .expect("kthread create: missing worker_private");
        worker_private.flags |= flags;
        drop(worker_private_guard);

        if flags.contains(KernelThreadFlags::IS_PER_CPU) {
            let cpu = (*self.bound_cpu.lock())
                .expect("kthread create: per-cpu thread missing target cpu");
            let allowed = pcb.sched_info().cpus_allowed();
            assert!(
                allowed.get(cpu).unwrap_or(false) && allowed.iter_cpu().count() == 1,
                "kthread create: per-cpu affinity was not installed before first run"
            );
        }
    }

    pub fn bound_cpu(&self) -> Option<ProcessorId> {
        *self.bound_cpu.lock()
    }

    pub fn flags(&self) -> KernelThreadFlags {
        *self.flags.lock()
    }
}

pub struct KernelThreadMechanism;

impl KernelThreadMechanism {
    pub fn init_stage1() {
        assert!(ProcessManager::current_pcb().raw_pid() == RawPid::new(0));
        info!("Initializing kernel thread mechanism stage1...");

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
            CloneFlags::CLONE_VM | CloneFlags::CLONE_FS | CloneFlags::CLONE_FILES,
        )
        .unwrap_or_else(|e| panic!("Failed to create initial kernel thread, error: {:?}", e));

        ProcessManager::current_pcb()
            .flags
            .get_mut()
            .remove(ProcessFlags::KTHREAD);

        drop(irq_guard);
        set_system_state(SystemState::Scheduling);
        info!("Initializing kernel thread mechanism stage1 complete");
    }

    pub fn init_stage2() {
        assert!(ProcessManager::current_pcb()
            .flags()
            .contains(ProcessFlags::KTHREAD));
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            info!("Initializing kernel thread mechanism stage2...");
            // 初始化kthreadd
            let closure = KernelThreadClosure::EmptyClosure((Box::new(Self::kthread_daemon), ()));
            let info = KernelThreadCreateInfo::new(closure, "kthreadd".to_string());
            info.set_to_mark_sleep(false)
                .expect("kthreadadd should be run first");
            Self::__inner_create(
                &info,
                CloneFlags::CLONE_VM | CloneFlags::CLONE_FS | CloneFlags::CLONE_FILES,
            )
            .expect("Failed to create kthread daemon");
            KTHREAD_DAEMON_READY.store(true, Ordering::Release);
            info!("Initialize kernel thread mechanism stage2 complete");
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
        while !KTHREAD_DAEMON_READY.load(Ordering::Acquire) {
            // 等待kthreadd启动
            spin_loop()
        }
        KTHREAD_CREATE_LIST.lock().push_back(info.clone());
        Self::notify_daemon();
        return info.poll_result();
    }

    /// Notify kthreadd after publishing create or zombie-reap work.
    pub(crate) fn notify_daemon() {
        KTHREAD_DAEMON_WORK_PENDING.store(true, Ordering::Release);
        KTHREAD_DAEMON_WAIT.wakeup(None);
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
            .unwrap_or_else(|_| panic!("Failed to wakeup kthread: {:?}", pcb.raw_pid()));
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
            pcb.raw_pid()
        );
        let exited_completion = {
            let kthread = worker_private
                .as_mut()
                .unwrap()
                .kernel_thread_mut()
                .expect("Error type of worker private");
            kthread.flags.insert(KernelThreadFlags::SHOULD_STOP);
            kthread.exited_completion()
        };

        drop(worker_private);

        ProcessManager::wakeup(pcb).ok();

        if let ProcessState::Exited(code) = pcb.sched_info().state() {
            return Ok(code);
        }

        exited_completion.wait_for_completion()?;

        let worker_private = pcb.worker_private();
        let result = worker_private
            .as_ref()
            .and_then(|x| x.kernel_thread())
            .map(|x| x.result())
            .unwrap_or_else(|| pcb.sched_info().state().exit_code().unwrap_or(0));
        Ok(result)
    }

    /// 请求一个内核线程退出（不等待其真正退出）
    #[allow(dead_code)]
    pub fn request_stop(pcb: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
        if !pcb.flags().contains(ProcessFlags::KTHREAD) {
            panic!("Cannt stop a non-kthread process");
        }

        let mut worker_private = pcb.worker_private();
        assert!(
            worker_private.is_some(),
            "kthread request_stop: worker_private is none, pid: {:?}",
            pcb.raw_pid()
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
        Ok(())
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
            pcb.raw_pid()
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
        {
            // 初始化worker_private
            let mut worker_private_guard = current_pcb.worker_private();
            let worker_private = WorkerPrivate::KernelThread(KernelThreadPcbPrivate::new());
            *worker_private_guard = Some(worker_private);
        }
        // 设置为kthread
        current_pcb.flags().insert(ProcessFlags::KTHREAD);
        let kthreadd_pcb = current_pcb.clone();
        drop(current_pcb);

        loop {
            KTHREAD_DAEMON_WAIT
                .wait_event_uninterruptible(
                    || KTHREAD_DAEMON_WORK_PENDING.swap(false, Ordering::AcqRel),
                    None::<fn()>,
                )
                .expect("kthreadd wait failed");

            loop {
                let Some(info) = KTHREAD_CREATE_LIST.lock().pop_front() else {
                    break;
                };
                // create a new kernel thread
                let result: Result<RawPid, SystemError> = Self::__inner_create(
                    &info,
                    CloneFlags::CLONE_VM | CloneFlags::CLONE_FS | CloneFlags::CLONE_FILES,
                );
                if result.is_err() {
                    // 创建失败
                    info.set_create_error();
                };
            }

            Self::reap_zombie_kthreads(&kthreadd_pcb);
        }
    }

    fn reap_zombie_kthreads(current_pcb: &Arc<ProcessControlBlock>) {
        let child_pids = {
            let guard = current_pcb.children_read_irqsave();
            guard.clone()
        };

        for pid in child_pids {
            let Some(task) = ProcessManager::find_task_by_vpid(pid) else {
                continue;
            };
            if !task.is_kthread() || !task.is_zombie() {
                continue;
            }
            if task.try_mark_dead_from_zombie() {
                unsafe {
                    ProcessManager::release(task.raw_pid());
                }
            }
            drop(task);
        }
    }
}

/// Exercise the externally visible kthread create/run/stop handshakes in a
/// real DragonOS guest. This is intentionally invoked only through debugfs;
/// production paths do not pay the stress-test cost.
pub(crate) fn run_debug_selftests() -> Result<String, SystemError> {
    if KTHREAD_SELFTEST_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(SystemError::EBUSY);
    }
    let _guard = KthreadSelftestGuard;

    let mut report = String::new();
    let mut failures = 0usize;

    append_kthread_selftest_case(
        &mut report,
        "create_stopped_stop",
        selftest_create_stopped_stop(),
        &mut failures,
    );
    append_kthread_selftest_case(
        &mut report,
        "create_and_run_stop",
        selftest_create_and_run_stop(),
        &mut failures,
    );

    let (quick_exit_ok, quick_exit_completed) = selftest_quick_exit(512);
    append_kthread_selftest_case(&mut report, "quick_exit_512", quick_exit_ok, &mut failures);
    report.push_str(&alloc::format!(
        "quick_exit_completed={quick_exit_completed}\n"
    ));

    if failures == 0 {
        report.insert_str(0, "status=ok\n");
    } else {
        report.insert_str(0, &alloc::format!("status=fail failures={failures}\n"));
    }
    Ok(report)
}

fn selftest_create_stopped_stop() -> bool {
    let entered = Arc::new(AtomicUsize::new(0));
    let entered_worker = entered.clone();
    let name = "kthread-selftest-stopped".to_string();
    let closure = KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            entered_worker.fetch_add(1, Ordering::AcqRel);
            0
        }),
        (),
    ));
    let Some(pcb) = KernelThreadMechanism::create(closure, name.clone()) else {
        return false;
    };

    let worker_private_ready = pcb
        .worker_private()
        .as_ref()
        .and_then(|private| private.kernel_thread())
        .is_some();
    let ready_ok = entered.load(Ordering::Acquire) == 0
        && pcb.sched_info().state() == ProcessState::Blocked(false)
        && pcb.flags().contains(ProcessFlags::KTHREAD)
        && pcb.basic().name() == name
        && worker_private_ready;
    let stop_ok = KernelThreadMechanism::stop(&pcb).is_ok();

    ready_ok && stop_ok && entered.load(Ordering::Acquire) == 0
}

fn selftest_create_and_run_stop() -> bool {
    const RESULT: i32 = 73;

    let entered = Arc::new(AtomicUsize::new(0));
    let entered_worker = entered.clone();
    let entered_completion = Arc::new(Completion::new());
    let entered_completion_worker = entered_completion.clone();
    let closure = KernelThreadClosure::EmptyClosure((
        Box::new(move || {
            entered_worker.fetch_add(1, Ordering::AcqRel);
            entered_completion_worker.complete();
            let current = ProcessManager::current_pcb();
            while !KernelThreadMechanism::should_stop(&current) {
                schedule(SchedMode::SM_NONE);
            }
            RESULT
        }),
        (),
    ));
    let Some(pcb) =
        KernelThreadMechanism::create_and_run(closure, "kthread-selftest-running".to_string())
    else {
        return false;
    };

    if entered_completion.wait_for_completion().is_err() {
        let _ = KernelThreadMechanism::stop(&pcb);
        return false;
    }
    let result = KernelThreadMechanism::stop(&pcb);
    entered.load(Ordering::Acquire) == 1 && result == Ok(RESULT as usize)
}

fn selftest_quick_exit(iterations: usize) -> (bool, usize) {
    const RESULT: i32 = 91;

    let completed = Arc::new(AtomicUsize::new(0));
    for _ in 0..iterations {
        let entered_completion = Arc::new(Completion::new());
        let entered_completion_worker = entered_completion.clone();
        let completed_worker = completed.clone();
        let closure = KernelThreadClosure::EmptyClosure((
            Box::new(move || {
                completed_worker.fetch_add(1, Ordering::AcqRel);
                entered_completion_worker.complete();
                RESULT
            }),
            (),
        ));
        let Some(pcb) = KernelThreadMechanism::create_and_run(
            closure,
            "kthread-selftest-quick-exit".to_string(),
        ) else {
            return (false, completed.load(Ordering::Acquire));
        };
        if entered_completion.wait_for_completion().is_err()
            || KernelThreadMechanism::stop(&pcb) != Ok(RESULT as usize)
        {
            return (false, completed.load(Ordering::Acquire));
        }
    }

    let completed = completed.load(Ordering::Acquire);
    (completed == iterations, completed)
}

fn append_kthread_selftest_case(report: &mut String, name: &str, ok: bool, failures: &mut usize) {
    if ok {
        report.push_str(&alloc::format!("{name}=ok\n"));
    } else {
        *failures += 1;
        report.push_str(&alloc::format!("{name}=fail\n"));
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
    let current = ProcessManager::current_pcb();

    // Complete all thread-visible setup before publishing Created. The arch
    // fork path may already have scheduled this child, so post-fork setup in
    // the parent cannot provide this ordering guarantee.
    current.set_name(info.name().clone());
    info.setup_pcb(&current);

    let closure: Box<KernelThreadClosure> = info.take_closure().unwrap();
    let to_mark_sleep = info.to_mark_sleep();

    if to_mark_sleep {
        // Match Linux kthread(): publish the stopped state before publishing
        // the creation result. A create_and_run wake racing with schedule()
        // then either keeps this current task runnable or re-enqueues it after
        // schedule has dequeued it; it can no longer be lost.
        let irq_guard = CurrentIrqArch::save_and_disable_irq();
        ProcessManager::mark_sleep(false).expect("Failed to mark sleep");
        info.set_create_ok(current.clone());
        drop(info);
        drop(irq_guard);
        schedule(SchedMode::SM_NONE);
    } else {
        info.set_create_ok(current.clone());
        drop(info);
    }
    drop(current);

    let mut retval = SystemError::EINTR.to_posix_errno();

    if !KernelThreadMechanism::should_stop(&ProcessManager::current_pcb()) {
        retval = closure.run();
    }

    let current = ProcessManager::current_pcb();
    {
        let mut worker_private = current.worker_private();
        let kthread = worker_private
            .as_mut()
            .and_then(|x| x.kernel_thread_mut())
            .expect("kthread exit: missing worker_private");
        kthread.set_result(retval as usize);
    }
    drop(current);

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
