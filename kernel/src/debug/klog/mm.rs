extern crate klog_types;

use core::intrinsics::unlikely;

use klog_types::{AllocatorLog, AllocatorLogType, LogSource, MMLogChannel};

use crate::{
    arch::CurrentTimeArch,
    process::{Pid, ProcessManager},
    time::TimeArch,
};

/// 全局的内存分配器日志通道
///
/// 标记为`no_mangle`是为了让调试器能够找到这个变量
#[no_mangle]
static __MM_ALLOCATOR_LOG_CHANNEL: MMLogChannel<{ MMDebugLogManager::MAX_ALLOC_LOG_NUM }> =
    MMLogChannel::new(MMDebugLogManager::MAX_ALLOC_LOG_NUM);

/// 全局的内存分配器日志id分配器
///
/// id从1开始, 因为0是无效的id
static __MM_DEBUG_LOG_IDA: ida::IdAllocator = ida::IdAllocator::new(1, usize::MAX);

/// 记录内存分配器的日志
///
/// ## 参数
///
/// - `log_type`：日志类型
/// - `source`：日志来源
pub fn mm_debug_log(log_type: AllocatorLogType, source: LogSource) {
    let pid = if unlikely(!ProcessManager::initialized()) {
        Some(Pid::new(0))
    } else {
        Some(ProcessManager::current_pcb().pid())
    };
    MMDebugLogManager::log(log_type, source, pid);
}

#[derive(Debug)]
pub(super) struct MMDebugLogManager;

impl MMDebugLogManager {
    /// 最大的内存分配器日志数量
    pub const MAX_ALLOC_LOG_NUM: usize = 100000;

    /// 记录内存分配器的日志
    ///
    /// ## 参数
    ///
    /// - `log_type`：日志类型
    /// - `source`：日志来源
    /// - `pid`：日志来源的pid
    pub fn log(log_type: AllocatorLogType, source: LogSource, pid: Option<Pid>) {
        let id = __MM_DEBUG_LOG_IDA.alloc().unwrap();
        let log = AllocatorLog::new(
            id as u64,
            log_type,
            source,
            pid.map(|p| p.data()),
            CurrentTimeArch::get_cycles() as u64,
        );

        let mut log = log;
        loop {
            let r = __MM_ALLOCATOR_LOG_CHANNEL.buf.push(log);
            if let Err(r) = r {
                // 如果日志通道满了，就把最早的日志丢弃
                if __MM_ALLOCATOR_LOG_CHANNEL.buf.remaining() == 0 {
                    __MM_ALLOCATOR_LOG_CHANNEL.buf.pop();
                }
                log = r.into_inner();
            } else {
                break;
            }
        }
    }
}
