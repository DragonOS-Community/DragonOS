use crate::driver::net::Iface;
use crate::init::initcall::INITCALL_SUBSYS;
use crate::libs::spinlock::{SpinLock, SpinLockGuard};
use crate::libs::wait_queue::WaitQueue;
use crate::process::kthread::{KernelThreadClosure, KernelThreadMechanism};
use crate::process::ProcessState;
use alloc::boxed::Box;
use alloc::string::ToString;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use system_error::SystemError;
use unified_init::macros::unified_init;

lazy_static! {
    //todo 按照软中断的做法，这里应该是每个CPU一个列表，但目前只实现单CPU版本
    static ref GLOBAL_NAPI_MANAGER: Arc<NapiManager> =
        NapiManager::new();
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

    pub fn poll(&self) -> bool {
        // log::info!("NAPI instance {} polling", self.napi_id);
        // 获取网卡的强引用
        if let Some(iface) = self.net_device.upgrade() {
            // 这里的weight原意是此次执行可以处理的包，如果超过了这个数就交给专门的内核线程(ksoftirqd)继续处理
            // 但目前我们就是在相当于ksoftirqd里面处理，如果在weight之内发现没数据包被处理了，在直接返回
            // 如果超过weight，返回true，表示还有工作没做完，会在下一次轮询继续处理
            // 因此语义是相同的
            for _ in 0..self.weight {
                if !iface.poll() {
                    return false;
                }
            }
        } else {
            log::error!(
                "NAPI instance {}: associated net device is gone",
                self.napi_id
            );
        }

        true
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
    loop {
        // 这里直接将全局的NAPI管理器的napi_list取出，清空全局的列表，避免占用锁时间过长
        let mut inner = GLOBAL_NAPI_MANAGER.inner();
        let mut poll_list = inner.napi_list.clone();
        inner.napi_list.clear();
        drop(inner);

        // log::info!("NAPI softirq processing {} instances", poll_list.len());

        // 如果此时长度为0,则让当前进程休眠，等待被唤醒
        if poll_list.is_empty() {
            GLOBAL_NAPI_MANAGER
                .inner()
                .has_pending_signal
                .store(false, Ordering::SeqCst);
        }

        while let Some(napi) = poll_list.pop() {
            let has_work_left = napi.poll();
            // log::info!("yes");

            if has_work_left {
                poll_list.push(napi);
            } else {
                napi_complete(napi);
            }
        }

        // log::info!("napi softirq iteration complete")

        // 在这种情况下，poll_list 中仍然有待处理的 NAPI 实例，压回队列，等待下一次唤醒时处理
        if !poll_list.is_empty() {
            GLOBAL_NAPI_MANAGER.inner().napi_list.extend(poll_list);
        }

        let _ = wq_wait_event_interruptible!(
            GLOBAL_NAPI_MANAGER.wait_queue(),
            GLOBAL_NAPI_MANAGER
                .inner()
                .has_pending_signal
                .load(Ordering::SeqCst),
            {}
        );
    }
}

/// 标记这个napi任务已经完成
pub fn napi_complete(napi: Arc<NapiStruct>) {
    napi.state
        .fetch_and(!NapiState::SCHED.bits(), Ordering::SeqCst);
}

/// 标记这个napi任务加入处理队列，已被调度
pub fn napi_schedule(napi: Arc<NapiStruct>) {
    let current_state = NapiState::from_bits_truncate(
        napi.state
            .fetch_or(NapiState::SCHED.bits(), Ordering::SeqCst),
    );

    if !current_state.contains(NapiState::SCHED) {
        let new_state = current_state.union(NapiState::SCHED);
        // log::info!("NAPI instance {} scheduled", napi.napi_id);
        napi.state.store(new_state.bits(), Ordering::SeqCst);
    }

    let mut inner = GLOBAL_NAPI_MANAGER.inner();
    inner.napi_list.push(napi);
    inner.has_pending_signal.store(true, Ordering::SeqCst);

    GLOBAL_NAPI_MANAGER.wakeup();

    // softirq_vectors().raise_softirq(SoftirqNumber::NetReceive);
}

pub struct NapiManager {
    inner: SpinLock<NapiManagerInner>,
    wait_queue: WaitQueue,
}

impl NapiManager {
    pub fn new() -> Arc<Self> {
        let inner = SpinLock::new(NapiManagerInner {
            has_pending_signal: AtomicBool::new(false),
            napi_list: Vec::new(),
        });
        Arc::new(Self {
            inner,
            wait_queue: WaitQueue::default(),
        })
    }

    pub fn inner(&self) -> SpinLockGuard<'_, NapiManagerInner> {
        self.inner.lock()
    }

    pub fn wait_queue(&self) -> &WaitQueue {
        &self.wait_queue
    }

    pub fn wakeup(&self) {
        self.wait_queue.wakeup(Some(ProcessState::Blocked(true)));
    }
}

pub struct NapiManagerInner {
    has_pending_signal: AtomicBool,
    napi_list: Vec<Arc<NapiStruct>>,
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
