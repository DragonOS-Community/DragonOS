/// 调度系统调用相关的工具函数
use crate::process::cred::CAPFlags;
use crate::process::ProcessControlBlock;

/// 检查当前进程是否有权限查询目标进程的调度信息
///
/// 权限规则（与 Linux 一致）：
/// - 进程自己可以查询
/// - 具有 CAP_SYS_NICE 权限的进程可以查询
/// - root 用户（uid == 0）可以查询
///
/// # Arguments
/// * `current_pcb` - 当前进程的 PCB
/// * `target_pcb` - 目标进程的 PCB
///
/// # Returns
/// * `true` - 有权限
/// * `false` - 无权限
pub fn has_sched_permission(
    current_pcb: &ProcessControlBlock,
    target_pcb: &ProcessControlBlock,
) -> bool {
    // 进程自己
    if current_pcb.raw_pid() == target_pcb.raw_pid() {
        return true;
    }

    let current_cred = current_pcb.cred();

    // 具有 CAP_SYS_NICE 权限
    if current_cred.has_capability(CAPFlags::CAP_SYS_NICE) {
        return true;
    }

    // root 用户（uid == 0）
    current_cred.uid.data() == 0
}

/// 检查当前进程是否有权限修改目标进程的 CPU affinity。
///
/// Linux 兼容语义：
/// - 进程自己总是允许
/// - 具有 CAP_SYS_NICE 的进程允许
/// - root（euid == 0）允许
/// - 同一用户（real/effective uid 匹配）允许
pub fn has_sched_setaffinity_permission(
    current_pcb: &ProcessControlBlock,
    target_pcb: &ProcessControlBlock,
) -> bool {
    if current_pcb.raw_pid() == target_pcb.raw_pid() {
        return true;
    }

    let current_cred = current_pcb.cred();
    if current_cred.has_capability(CAPFlags::CAP_SYS_NICE) {
        return true;
    }

    if current_cred.euid.data() == 0 {
        return true;
    }

    let target_cred = target_pcb.cred();
    current_cred.euid == target_cred.euid
        || current_cred.euid == target_cred.uid
        || current_cred.uid == target_cred.euid
        || current_cred.uid == target_cred.uid
}
