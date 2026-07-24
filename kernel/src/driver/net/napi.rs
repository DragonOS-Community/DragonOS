use crate::driver::net::{types::InterfaceFlags, Iface};
use crate::init::initcall::INITCALL_SUBSYS;
use crate::libs::spinlock::{SpinLock, SpinLockGuard};
use crate::libs::wait_queue::WaitQueue;
use crate::process::kthread::{KernelThreadClosure, KernelThreadMechanism};
use crate::process::ProcessState;
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::string::ToString;
use alloc::sync::{Arc, Weak};
use core::sync::atomic::AtomicU32;
use napi_state::CompleteState;
use system_error::SystemError;
use unified_init::macros::unified_init;

lazy_static! {
    //todo 按照软中断的做法，这里应该是每个CPU一个列表，但目前只实现单CPU版本
    static ref GLOBAL_NAPI_MANAGER: Arc<NapiManager> = NapiManager::new();
}

/// # NAPI 结构体
///
/// https://elixir.bootlin.com/linux/v6.13/source/include/linux/netdevice.h#L359
#[derive(Debug)]
pub struct NapiStruct {
    /// NAPI实例状态
    pub state: AtomicU32,
    /// NAPI实例权重，表示每次轮询时处理的最大包数
    pub weight: usize,
    /// 唯一id
    pub napi_id: usize,
    /// 指向所属网卡的弱引用
    pub net_device: Weak<dyn Iface>,
}

impl NapiStruct {
    pub fn new(net_device: Arc<dyn Iface>, weight: usize) -> Arc<Self> {
        Arc::new(Self {
            state: AtomicU32::new(NapiState::empty().bits()),
            weight,
            napi_id: net_device.nic_id(),
            net_device: Arc::downgrade(&net_device),
        })
    }

    fn poll(&self, budget: usize) -> Option<(Arc<dyn Iface>, NapiPollResult)> {
        if let Some(iface) = self.net_device.upgrade() {
            if !iface.flags().contains(InterfaceFlags::UP) {
                return Some((iface, NapiPollResult::idle()));
            }
            let result = iface.poll_napi(budget);
            return Some((iface, result));
        } else {
            log::error!(
                "NAPI instance {}: associated net device is gone",
                self.napi_id
            );
        }
        None
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NapiPollResult {
    pub work_done: usize,
    pub poll_again: bool,
}

impl NapiPollResult {
    pub const fn new(work_done: usize, poll_again: bool) -> Self {
        Self {
            work_done,
            poll_again,
        }
    }

    pub const fn idle() -> Self {
        Self::new(0, false)
    }
}

bitflags! {
    /// # NAPI状态标志
    ///
    /// https://elixir.bootlin.com/linux/v6.13/source/include/linux/netdevice.h#L398
    pub struct NapiState:u32{
        /// Poll is scheduled. 这是最核心的状态，表示NAPI实例已被调度，
        /// 存在于某个CPU的poll_list中等待处理。
        const SCHED             = 1 << 0;
        /// Missed a poll. 如果在NAPI实例被调度后但在实际处理前又有新的数据到达，
        const MISSED            = 1 << 1;
        /// Disable pending. NAPI正在被禁用，不应再被调度。
        const DISABLE           = 1 << 2;
        const NPSVC             = 1 << 3;
        /// NAPI added to system lists. 表示NAPI实例已注册到设备中。
        const LISTED            = 1 << 4;
        const NO_BUSY_POLL      = 1 << 5;
        const IN_BUSY_POLL      = 1 << 6;
        const PREFER_BUSY_POLL  = 1 << 7;
        /// The poll is performed inside its own thread.
        /// 一个可选的高级功能，表示此NAPI由专用内核线程处理。
        const THREADED          = 1 << 8;
        const SCHED_THREADED    = 1 << 9;
    }
}

#[inline(never)]
#[unified_init(INITCALL_SUBSYS)]
pub fn napi_init() -> Result<(), SystemError> {
    // 软中断做法
    // let napi_handler = Arc::new(NapiSoftirq::default());
    // softirq_vectors()
    //     .register_softirq(SoftirqNumber::NetReceive, napi_handler)
    //     .expect("Failed to register napi softirq");

    // 软中断的方式无法唤醒 :(
    // 使用一个专门的内核线程来处理NAPI轮询，模拟软中断的行为，相当于ksoftirq :)

    let closure: Box<dyn Fn() -> i32 + Send + Sync + 'static> = Box::new(move || {
        net_rx_action();
        0
    });
    let closure = KernelThreadClosure::EmptyClosure((closure, ()));
    let name = "napi_handler".to_string();
    let _pcb = KernelThreadMechanism::create_and_run(closure, name)
        .ok_or("")
        .expect("create napi_handler thread failed");

    log::info!("napi initialized successfully");
    Ok(())
}

fn net_rx_action() {
    const NET_RX_PACKET_BUDGET: usize = 300;
    const NET_RX_POLL_BUDGET: usize = 64;
    const NET_RX_TIME_BUDGET_US: i64 = 2_000;

    loop {
        let mut inner = GLOBAL_NAPI_MANAGER.inner();
        let mut active = core::mem::take(&mut inner.napi_list);
        drop(inner);

        let started_at = crate::time::Instant::now().total_micros();
        let mut packets_left = NET_RX_PACKET_BUDGET;
        let mut polls_left = NET_RX_POLL_BUDGET;
        let mut repoll = VecDeque::new();

        while let Some(napi) = active.pop_front() {
            let elapsed = crate::time::Instant::now().total_micros() - started_at;
            if polls_left == 0 || packets_left == 0 || elapsed >= NET_RX_TIME_BUDGET_US {
                active.push_front(napi);
                break;
            }

            let budget = core::cmp::min(napi.weight, packets_left);
            polls_left -= 1;

            let Some((iface, result)) = napi.poll(budget) else {
                // A registered NAPI must not outlive its interface. Clear the state to avoid
                // retaining an owner forever; device teardown is responsible for stopping DMA.
                napi_complete(napi);
                continue;
            };

            debug_assert!(result.work_done <= budget);
            packets_left = packets_left.saturating_sub(core::cmp::min(result.work_done, budget));

            if result.poll_again || result.work_done >= budget {
                repoll.push_back(napi);
            } else {
                iface.napi_complete(napi);
            }
        }

        let mut inner = GLOBAL_NAPI_MANAGER.inner();
        // Fair merge order: work which did not get a turn, newly scheduled work, then busy repolls.
        active.append(&mut inner.napi_list);
        active.append(&mut repoll);
        inner.napi_list = active;

        if !inner.napi_list.is_empty() {
            inner.has_pending_signal = true;
            drop(inner);
            crate::sched::sched_yield();
            continue;
        }

        inner.has_pending_signal = false;
        drop(inner);

        let _ = wq_wait_event_interruptible!(
            GLOBAL_NAPI_MANAGER.wait_queue(),
            GLOBAL_NAPI_MANAGER.inner().has_pending_signal,
            {}
        );
    }
}

pub(crate) fn napi_complete_state(napi: &NapiStruct) -> CompleteState {
    napi_state::complete(&napi.state)
}

/// Complete a NAPI instance which has no device-specific callback handshake.
pub fn napi_complete(napi: Arc<NapiStruct>) {
    if napi_complete_state(&napi) == CompleteState::Missed {
        __napi_schedule(napi);
    }
}

/// Permanently stop scheduling a NAPI instance whose device can no longer be
/// polled. This must be a single atomic transition: completing twice can race
/// a scheduler and orphan its newly acquired owner.
pub(crate) fn napi_disable(napi: &NapiStruct) {
    napi_state::disable(&napi.state);
}

pub(crate) fn napi_schedule_prep(napi: &NapiStruct) -> bool {
    napi_state::schedule_prep(&napi.state)
}

pub(crate) fn __napi_schedule(napi: Arc<NapiStruct>) {
    debug_assert_ne!(
        napi.state.load(core::sync::atomic::Ordering::Relaxed) & napi_state::SCHED,
        0
    );
    let mut inner = GLOBAL_NAPI_MANAGER.inner();
    inner.napi_list.push_back(napi);
    inner.has_pending_signal = true;
    drop(inner);

    GLOBAL_NAPI_MANAGER.wakeup();
}

/// Acquire a NAPI owner, let the device mask callbacks, then publish it once.
pub fn napi_schedule(napi: Arc<NapiStruct>) {
    if !napi_schedule_prep(&napi) {
        return;
    }

    let Some(iface) = napi.net_device.upgrade() else {
        log::error!(
            "NAPI instance {} scheduled after its interface was removed",
            napi.napi_id
        );
        napi_disable(&napi);
        return;
    };
    iface.napi_poll_begin();
    __napi_schedule(napi);
}

pub struct NapiManager {
    inner: SpinLock<NapiManagerInner>,
    wait_queue: WaitQueue,
}

impl NapiManager {
    pub fn new() -> Arc<Self> {
        let inner = SpinLock::new(NapiManagerInner {
            has_pending_signal: false,
            napi_list: VecDeque::new(),
        });
        Arc::new(Self {
            inner,
            wait_queue: WaitQueue::default(),
        })
    }

    pub fn inner(&self) -> SpinLockGuard<'_, NapiManagerInner> {
        // 必须使用 lock_irqsave() 关闭中断，因为 napi_schedule() 可能在中断上下文中被调用
        // 如果使用普通的 lock()，当内核线程持有锁时发生中断，中断处理程序试图获取同一把锁会死锁
        self.inner.lock_irqsave()
    }

    pub fn wait_queue(&self) -> &WaitQueue {
        &self.wait_queue
    }

    pub fn wakeup(&self) {
        self.wait_queue.wakeup(Some(ProcessState::Blocked(true)));
    }
}

pub struct NapiManagerInner {
    has_pending_signal: bool,
    napi_list: VecDeque<Arc<NapiStruct>>,
}

// 下面的是软中断的做法，无法唤醒，做个记录

// #[derive(Debug)]
// pub struct NapiSoftirq {
//     running: AtomicBool,
// }

// impl Default for NapiSoftirq {
//     fn default() -> Self {
//         Self {
//             running: AtomicBool::new(false),
//         }
//     }
// }

// impl SoftirqVec for NapiSoftirq {
//     fn run(&self) {
//         log::info!("NAPI softirq running");
//         if self
//             .running
//             .compare_exchange(
//                 false,
//                 true,
//                 core::sync::atomic::Ordering::SeqCst,
//                 core::sync::atomic::Ordering::SeqCst,
//             )
//             .is_ok()
//         {
//             net_rx_action();
//             self.running
//                 .store(false, core::sync::atomic::Ordering::SeqCst);
//         } else {
//             log::warn!("NAPI softirq is already running");
//         }
//     }
// }
