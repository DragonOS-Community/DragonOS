extern crate klog_types;

use core::sync::atomic::{compiler_fence, Ordering};

use klog_types::{AllocatorLog, AllocatorLogType, LogSource, MMLogChannel};

use crate::{arch::CurrentTimeArch, libs::spinlock::SpinLock, process::Pid, time::TimeArch};

/// 全局的内存分配器日志通道
///
/// 标记为`no_mangle`是为了让调试器能够找到这个变量
#[no_mangle]
static __MM_ALLOCATOR_LOG_CHANNEL: MMLogChannel<{ MMDebugLogManager::MAX_ALLOC_LOG_NUM }> =
    MMLogChannel::new(MMDebugLogManager::MAX_ALLOC_LOG_NUM);

/// 全局的内存分配器日志id分配器
///
/// id从1开始, 因为0是无效的id
static __MM_DEBUG_LOG_IDA: SpinLock<ida::IdAllocator> =
    SpinLock::new(ida::IdAllocator::new(1, usize::MAX).unwrap());

/// 记录内存分配器的日志
///
/// ## 参数
///
/// - `log_type`：日志类型
/// - `source`：日志来源
pub fn mm_debug_log(_log_type: AllocatorLogType, _source: LogSource) {
    // todo: 由于目前底层的thingbuf存在卡死的问题，因此这里暂时注释掉。
    // let pid = if unlikely(!ProcessManager::initialized()) {
    //     Some(Pid::new(0))
    // } else {
    //     Some(ProcessManager::current_pcb().pid())
    // };
    // MMDebugLogManager::log(log_type, source, pid);
}

#[derive(Debug)]
pub(super) struct MMDebugLogManager;

impl MMDebugLogManager {
    /// 最大的内存分配器日志数量
    pub const MAX_ALLOC_LOG_NUM: usize = 10000;

    /// 记录内存分配器的日志
    ///
    /// ## 参数
    ///
    /// - `log_type`：日志类型
    /// - `source`：日志来源
    /// - `pid`：日志来源的pid
    #[allow(dead_code)]
    pub fn log(log_type: AllocatorLogType, source: LogSource, pid: Option<Pid>) {
        let id = __MM_DEBUG_LOG_IDA.lock_irqsave().alloc().unwrap();
        let log = AllocatorLog::new(
            id as u64,
            log_type,
            source,
            pid.map(|p| p.data()),
            CurrentTimeArch::get_cycles() as u64,
        );

        let mut log = log;
        loop {
            compiler_fence(Ordering::SeqCst);
            let r = __MM_ALLOCATOR_LOG_CHANNEL.buf.push(log);
            compiler_fence(Ordering::SeqCst);
            if let Err(r) = r {
                // 如果日志通道满了，就把最早的日志丢弃
                if __MM_ALLOCATOR_LOG_CHANNEL.buf.remaining() == 0 {
                    compiler_fence(Ordering::SeqCst);
                    __MM_ALLOCATOR_LOG_CHANNEL.buf.pop();
                    compiler_fence(Ordering::SeqCst);
                }
                log = r.into_inner();
                compiler_fence(Ordering::SeqCst);
            } else {
                break;
            }
        }
    }
}
