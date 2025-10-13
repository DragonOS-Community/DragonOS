use alloc::{string::{String, ToString}, sync::{Arc, Weak}, vec::Vec};
use system_error::SystemError;
use log::debug;

use crate::{
    filesystem::{
        kernfs::dynamic::DynamicLookup,
        vfs::IndexNode,
    },
    process::{namespace::pid_namespace::PidNamespace, RawPid},
};

use super::ProcFS;

/// ProcFS 的动态 PID 目录查找实现
#[derive(Debug)]
pub struct ProcFSDynamicPidLookup {
    pid_ns: Arc<PidNamespace>,
    procfs: Weak<ProcFS>,
}

impl ProcFSDynamicPidLookup {
    pub fn new(pid_ns: Arc<PidNamespace>, procfs: Weak<ProcFS>) -> Self {
        Self { pid_ns, procfs }
    }

    /// 检查给定的名称是否是一个有效的 PID
    fn is_pid_name(&self, name: &str) -> Option<u32> {
        name.parse::<u32>().ok()
    }
}

impl DynamicLookup for ProcFSDynamicPidLookup {
    fn is_valid_entry(&self, name: &str) -> bool {
        // 仅当名称为数字时，按"挂载 pidns 的 nspid"解析
        let Some(ns_pid_num) = self.is_pid_name(name) else {
            return false;
        };

        // 检查在挂载的 pidns 中是否存在该 nspid 对应的进程
        let raw = RawPid::new(ns_pid_num as usize);
        
        if let Some(pid) = self.pid_ns.find_pid_in_ns(raw) {
            // 直接在当前命名空间中找到了
            if let Some(task) = pid.pid_task(crate::process::pid::PidType::PID) {
                // 检查进程是否已经退出
                let state = task.sched_info().inner_lock_read_irqsave().state();
                return !state.is_exited();
            }
        } else {
            // 在当前命名空间中没有直接找到，检查是否有进程在此命名空间中应该显示为该 PID
            // 特别是检查 PID 1，它通常是命名空间的 init 进程
            if ns_pid_num == 1 {
                if let Some(child_reaper) = self.pid_ns.child_reaper() {
                    if let Some(child_reaper_pcb) = child_reaper.upgrade() {
                        // 检查 child_reaper 是否已经退出
                        let state = child_reaper_pcb.sched_info().inner_lock_read_irqsave().state();
                        return !state.is_exited();
                    }
                }
            }
        }

        false
    }

    fn dynamic_find(&self, name: &str) -> Result<Option<Arc<dyn IndexNode>>, SystemError> {
        // 仅当名称为数字时，按“挂载 pidns 的 nspid”解析
        let Some(ns_pid_num) = self.is_pid_name(name) else {
            return Ok(None);
        };
        // debug!("ProcFSDynamicPidLookup::dynamic_find: Attempting to find nspid {} in mounted pidns", ns_pid_num);

        // 首先检查在挂载的 pidns 中是否直接存在该 nspid
        let raw = RawPid::new(ns_pid_num as usize);
        let mut found_process_id = None;
        
        if let Some(pid) = self.pid_ns.find_pid_in_ns(raw) {
            // 直接在当前命名空间中找到了
            if let Some(task) = pid.pid_task(crate::process::pid::PidType::PID) {
                // 检查进程是否已经退出
                let state = task.sched_info().inner_lock_read_irqsave().state();
                if !state.is_exited() {
                    found_process_id = Some(task.raw_pid());
                } else {
                    debug!("ProcFSDynamicPidLookup::dynamic_find: Process {} has exited, not creating directory", ns_pid_num);
                    return Ok(None);
                }
            }
        } else {
            // 在当前命名空间中没有直接找到，检查是否有进程在此命名空间中应该显示为该 PID
            // 特别是检查 PID 1，它通常是命名空间的 init 进程
            if ns_pid_num == 1 {
                if let Some(child_reaper) = self.pid_ns.child_reaper() {
                    if let Some(child_reaper_pcb) = child_reaper.upgrade() {
                        // 检查 child_reaper 是否已经退出
                        let state = child_reaper_pcb.sched_info().inner_lock_read_irqsave().state();
                        if !state.is_exited() {
                            found_process_id = Some(child_reaper_pcb.raw_pid());
                            debug!("ProcFSDynamicPidLookup::dynamic_find: Found child_reaper for PID 1 in namespace");
                        } else {
                            debug!("ProcFSDynamicPidLookup::dynamic_find: Child reaper for PID 1 has exited");
                            return Ok(None);
                        }
                    }
                }
            }
        }

        if found_process_id.is_none() {
            debug!("ProcFSDynamicPidLookup::dynamic_find: nspid {} not present in mounted pidns", ns_pid_num);
            return Ok(None);
        }

        // 使用 ProcFS 实例动态创建以 nspid 为名的临时目录
        if let Some(procfs) = self.procfs.upgrade() {
            let process_id = found_process_id.unwrap();
            match procfs.create_temporary_process_directory(process_id, &ns_pid_num.to_string()) {
                Ok(pid_dir) => {
                    // debug!("ProcFSDynamicPidLookup::dynamic_find: Created temporary directory for nspid {}", ns_pid_num);
                    return Ok(Some(pid_dir));
                }
                Err(e) => {
                    // 非致命：返回 None 以便 readdir/lookup 跳过
                    debug!("ProcFSDynamicPidLookup::dynamic_find: Failed to create temporary directory for nspid {}: {:?}", ns_pid_num, e);
                    return Ok(None);
                }
            }
        }

        Ok(None)
    }

    fn dynamic_list(&self) -> Result<Vec<String>, SystemError> {
        // 基于挂载的 pidns 枚举当前可见的所有 nspid
        let mut entries: Vec<String> = Vec::new();

        let mut ns_pids = self.pid_ns.get_all_pids();
        // debug!("ProcFSDynamicPidLookup::dynamic_list: Found {} total nspids in mounted pidns", ns_pids.len());

        // 排序保证稳定输出
        ns_pids.sort_by_key(|raw| raw.data());
        
        for raw in ns_pids {
            // 仅列出仍存在且未退出的条目
            if let Some(pid) = self.pid_ns.find_pid_in_ns(raw) {
                if let Some(task) = pid.pid_task(crate::process::pid::PidType::PID) {
                    // 检查进程是否已经退出
                    let state = task.sched_info().inner_lock_read_irqsave().state();
                    if !state.is_exited() {
                        entries.push(raw.data().to_string());
                        // debug!("ProcFSDynamicPidLookup::dynamic_list: Added nspid {} to list", raw.data());
                    } else {
                    // debug!("ProcFSDynamicPidLookup::dynamic_list: Skipping exited nspid {}", raw.data());
                    // 进程已退出，目录将在进程退出时被立即清理，这里不需要处理
                }
                } else {
                    debug!("ProcFSDynamicPidLookup::dynamic_list: No task found for nspid {}", raw.data());
                }
            }
        }
        // ::log::info!(
        //     "ProcFSDynamicPidLookup::dynamic_list: live nspids at ns level {} => {:?}",
        //     self.pid_ns.level(),
        //     entries
        // );

        // 特殊处理：如果命名空间有 child_reaper（init 进程），且还没有 PID 1，则添加它
        if !entries.contains(&"1".to_string()) {
            if let Some(child_reaper) = self.pid_ns.child_reaper() {
                if let Some(child_reaper_pcb) = child_reaper.upgrade() {
                    // 检查 child_reaper 是否已经退出
                    let state = child_reaper_pcb.sched_info().inner_lock_read_irqsave().state();
                    if !state.is_exited() {
                        entries.push("1".to_string());
                        debug!("ProcFSDynamicPidLookup::dynamic_list: Added nspid 1 (child_reaper) to list");
                    } else {
                        debug!("ProcFSDynamicPidLookup::dynamic_list: Child reaper for PID 1 has exited, not adding");
                    }
                }
            }
        }

        // 重新排序以确保 PID 1 在正确位置
        entries.sort_by_key(|s| s.parse::<u32>().unwrap_or(0));

        Ok(entries)
    }

}



impl ProcFSDynamicPidLookup {
    /// 检查进程是否在当前命名空间中可见
    #[allow(dead_code)]
    fn is_process_visible_in_namespace(&self, process_ns: &Arc<PidNamespace>) -> bool {
        // Linux PID 命名空间可见性规则：
        // 1. 进程在当前命名空间中 -> 可见
        // 2. 进程在当前命名空间的子命名空间中 -> 可见
        // 3. 进程在当前命名空间的父命名空间中 -> 不可见
        
        let current_level = self.pid_ns.level();
        let process_level = process_ns.level();
        
        debug!("ProcFSDynamicPidLookup::is_process_visible: current_ns_level={}, process_ns_level={}", 
               current_level, process_level);
        
        // 检查当前命名空间是否是进程命名空间的祖先（或相同）
        let mut check_ns = Some(process_ns.clone());
        while let Some(ns) = check_ns {
            if Arc::ptr_eq(&ns, &self.pid_ns) {
                debug!("ProcFSDynamicPidLookup::is_process_visible: VISIBLE - found matching namespace");
                return true;
            }
            check_ns = ns.parent();
        }
        
        debug!("ProcFSDynamicPidLookup::is_process_visible: NOT VISIBLE - no matching namespace found");
        false
    }
}