//! ProcFS per-pidns instance registry and notifications

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};

use crate::{
    libs::spinlock::SpinLock,
    process::{ProcessManager, namespace::pid_namespace::PidNamespace},
};

use super::{
    data::process_info::ProcessId,
    fs::ProcFS,
};

/* ================= ProcFS per-pidns instance registry ================= */

struct ProcRegister {
    // key: pidns 唯一键（此处用指针地址；如有 ns.inum 可替换）
    map: BTreeMap<usize, Vec<Weak<ProcFS>>>,
}

impl ProcRegister {
    const fn new() -> Self {
        Self { map: BTreeMap::new() }
    }

    #[inline]
    fn key_of(ns: &Arc<PidNamespace>) -> usize {
        Arc::as_ptr(ns) as usize
    }

    fn register(&mut self, ns: &Arc<PidNamespace>, inst: &Arc<ProcFS>) {
        let k = Self::key_of(ns);
        self.map.entry(k).or_default().push(Arc::downgrade(inst));
    }

    fn instances_of(&mut self, ns: &Arc<PidNamespace>) -> Vec<Arc<ProcFS>> {
        let k = Self::key_of(ns);
        let mut out = Vec::new();
        if let Some(v) = self.map.get_mut(&k) {
            v.retain(|w| {
                if let Some(s) = w.upgrade() {
                    out.push(s);
                    true
                } else {
                    false
                }
            });
        }
        out
    }
}

static PROC_REGISTER: SpinLock<ProcRegister> = SpinLock::new(ProcRegister::new());

/// 将一个 ProcFS 挂载实例登记到注册表（建议在 produce_fs 创建 Arc<ProcFS> 后调用）
pub fn proc_register_instance(inst: &Arc<ProcFS>) {
    let ns = inst.pid_namespace().clone();
    let mut g = PROC_REGISTER.lock();
    g.register(&ns, inst);
}

/// 查询某个 pidns 下所有已挂载的 ProcFS 实例（自动剔除已失效的实例）
pub fn proc_instances_of(ns: &Arc<PidNamespace>) -> Vec<Arc<ProcFS>> {
    let mut g = PROC_REGISTER.lock();
    g.instances_of(ns)
}

/// 进程退出通知：按该任务所属 pidns（未来可扩展为祖先链）定向清理所有挂载的 /proc/<pid>
pub fn proc_notify_pid_exit(pid: ProcessId) {
    // ::log::info!(
    //     "proc_notify_pid_exit: START for global PID {}",
    //     pid.data()
    // );
    
    if !ProcessManager::initialized() {
        ::log::warn!(
            "proc_notify_pid_exit: ProcessManager not initialized, skipping cleanup for PID {}",
            pid.data()
        );
        return;
    }
    
    if let Some(pcb) = ProcessManager::find(pid) {
        ::log::info!(
            "proc_notify_pid_exit: global PID {} exiting; active ns level {}",
            pid.data(),
            pcb.active_pid_ns().level()
        );
        // 从该进程所属的活跃 pidns 向上遍历祖先链，逐级通知所有挂载实例
        let mut cur = Some(pcb.active_pid_ns());
        while let Some(ns) = cur {
            let instances = proc_instances_of(&ns);
            ::log::info!(
                "proc_notify_pid_exit: notifying ns level {} with {} instances",
                ns.level(),
                instances.len()
            );
            for inst in instances {
                ::log::info!(
                    "proc_notify_pid_exit: -> instance ns level {}",
                    inst.pid_namespace().level()
                );
                match inst.remove_process_directory(pid) {
                    Ok(_) => ::log::info!(
                        "proc_notify_pid_exit: remove call Ok for pid {} on ns level {}",
                        pid.data(),
                        inst.pid_namespace().level()
                    ),
                    Err(e) => ::log::error!(
                        "proc_notify_pid_exit: remove call Err({:?}) for pid {} on ns level {}",
                        e,
                        pid.data(),
                        inst.pid_namespace().level()
                    ),
                }
            }
            // 向上走到父命名空间；若无父，则结束
            cur = ns.parent();
        }
    } else {
        // ::log::warn!(
        //     "proc_notify_pid_exit: Process {} not found in ProcessManager",
        //     pid.data()
        // );
    }
    
    // ::log::info!(
    //     "proc_notify_pid_exit: COMPLETED for global PID {}",
    //     pid.data()
    // );
}

/// 进程创建（或首次可见）通知：在本 pidns 下的所有 /proc 挂载实例创建 /proc/<pid> 目录
/// 进程创建通知：在纯动态模式下，只记录日志，不自动创建目录
/// 所有进程目录将通过动态查找按需创建
pub fn proc_notify_pid_register(pid: ProcessId) {
    if !ProcessManager::initialized() {
        return;
    }
    if let Some(pcb) = ProcessManager::find(pid) {
        // 从该进程的活跃 pidns 向上遍历祖先链，记录可见的命名空间实例
        let mut cur = Some(pcb.active_pid_ns());
        while let Some(ns) = cur {
            let _instances = proc_instances_of(&ns);
            // ::log::info!("proc_notify_pid_register: PID {} in namespace level {}, found {} instances (pure dynamic mode - no auto-creation)", 
            //              pid.data(), ns.level(), _instances.len());
            
            // 在纯动态模式下，我们不再自动创建目录
            // 目录将在用户访问时通过动态查找机制创建
            
            cur = ns.parent();
        }
    } else {
        ::log::warn!("proc_notify_pid_register: Process {} not found", pid.data());
    }
}

/* ================= end registry ================= */


