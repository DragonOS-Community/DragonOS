#![allow(dead_code)]
//! Memory Cgroup Subsystem for DragonOS
//! 
//! This module implements the memory cgroup subsystem, which provides
//! memory usage tracking and limiting functionality for cgroups.
//! 
//! The memory cgroup subsystem allows:
//! - Memory usage tracking per cgroup
//! - Memory limits enforcement
//! - Memory reclaim when limits are exceeded
//! - Memory statistics reporting

use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    sync::atomic::{AtomicU64, Ordering},
};
use system_error::SystemError;

use crate::{
    libs::{
        rwlock::{RwLock, RwLockReadGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
    process::RawPid,
};

use super::{
    CfType, CgroupSubsystem, CgroupSubsysId, CgroupSubsysState, 
    cgroup_manager,
};

/// Memory cgroup subsystem ID
pub const MEM_CGROUP_SUBSYS_ID: CgroupSubsysId = 0;

/// Memory cgroup statistics
#[derive(Debug, Default)]
pub struct MemCgroupStats {
    /// Current memory usage in bytes
    pub usage: AtomicU64,
    
    /// Peak memory usage in bytes
    pub max_usage: AtomicU64,
    
    /// Memory limit in bytes
    pub limit: AtomicU64,
    
    /// Soft memory limit in bytes
    pub soft_limit: AtomicU64,
    
    /// Number of page faults
    pub page_faults: AtomicU64,
    
    /// Number of major page faults
    pub major_page_faults: AtomicU64,
    
    /// Anonymous memory usage
    pub anon_usage: AtomicU64,
    
    /// File-backed memory usage
    pub file_usage: AtomicU64,
    
    /// Kernel memory usage
    pub kernel_usage: AtomicU64,
    
    /// Swap usage
    pub swap_usage: AtomicU64,
    
    /// Number of OOM kills
    pub oom_kills: AtomicU64,
}

impl MemCgroupStats {
    /// Create new memory cgroup statistics
    pub fn new() -> Self {
        Self {
            usage: AtomicU64::new(0),
            max_usage: AtomicU64::new(0),
            limit: AtomicU64::new(u64::MAX), // No limit by default
            soft_limit: AtomicU64::new(u64::MAX),
            page_faults: AtomicU64::new(0),
            major_page_faults: AtomicU64::new(0),
            anon_usage: AtomicU64::new(0),
            file_usage: AtomicU64::new(0),
            kernel_usage: AtomicU64::new(0),
            swap_usage: AtomicU64::new(0),
            oom_kills: AtomicU64::new(0),
        }
    }
    
    /// Get current memory usage
    pub fn usage(&self) -> u64 {
        self.usage.load(Ordering::SeqCst)
    }
    
    /// Get memory limit
    pub fn limit(&self) -> u64 {
        self.limit.load(Ordering::SeqCst)
    }
    
    /// Set memory limit
    pub fn set_limit(&self, limit: u64) {
        self.limit.store(limit, Ordering::SeqCst);
    }
    
    /// Get soft memory limit
    pub fn soft_limit(&self) -> u64 {
        self.soft_limit.load(Ordering::SeqCst)
    }
    
    /// Set soft memory limit
    pub fn set_soft_limit(&self, limit: u64) {
        self.soft_limit.store(limit, Ordering::SeqCst);
    }
    
    /// Add memory usage
    pub fn add_usage(&self, bytes: u64) -> Result<(), SystemError> {
        let old_usage = self.usage.fetch_add(bytes, Ordering::SeqCst);
        let new_usage = old_usage + bytes;
        
        // Update max usage
        let mut max_usage = self.max_usage.load(Ordering::SeqCst);
        while new_usage > max_usage {
            match self.max_usage.compare_exchange_weak(
                max_usage,
                new_usage,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(current) => max_usage = current,
            }
        }
        
        // Check limit
        let limit = self.limit();
        if new_usage > limit {
            // Revert the usage increase
            self.usage.fetch_sub(bytes, Ordering::SeqCst);
            return Err(SystemError::ENOMEM);
        }
        
        Ok(())
    }
    
    /// Subtract memory usage
    pub fn sub_usage(&self, bytes: u64) {
        self.usage.fetch_sub(bytes, Ordering::SeqCst);
    }
    
    /// Add anonymous memory usage
    pub fn add_anon_usage(&self, bytes: u64) {
        self.anon_usage.fetch_add(bytes, Ordering::SeqCst);
    }
    
    /// Subtract anonymous memory usage
    pub fn sub_anon_usage(&self, bytes: u64) {
        self.anon_usage.fetch_sub(bytes, Ordering::SeqCst);
    }
    
    /// Add file memory usage
    pub fn add_file_usage(&self, bytes: u64) {
        self.file_usage.fetch_add(bytes, Ordering::SeqCst);
    }
    
    /// Subtract file memory usage
    pub fn sub_file_usage(&self, bytes: u64) {
        self.file_usage.fetch_sub(bytes, Ordering::SeqCst);
    }
    
    /// Add kernel memory usage
    pub fn add_kernel_usage(&self, bytes: u64) {
        self.kernel_usage.fetch_add(bytes, Ordering::SeqCst);
    }
    
    /// Subtract kernel memory usage
    pub fn sub_kernel_usage(&self, bytes: u64) {
        self.kernel_usage.fetch_sub(bytes, Ordering::SeqCst);
    }
    
    /// Increment page fault count
    pub fn inc_page_faults(&self) {
        self.page_faults.fetch_add(1, Ordering::SeqCst);
    }
    
    /// Increment major page fault count
    pub fn inc_major_page_faults(&self) {
        self.major_page_faults.fetch_add(1, Ordering::SeqCst);
    }
    
    /// Increment OOM kill count
    pub fn inc_oom_kills(&self) {
        self.oom_kills.fetch_add(1, Ordering::SeqCst);
    }
}

/// Memory cgroup data
#[derive(Debug)]
pub struct MemCgroup {
    /// Memory statistics
    stats: MemCgroupStats,
    
    /// Parent memory cgroup
    parent: RwLock<Option<Arc<MemCgroup>>>,
    
    /// Children memory cgroups
    children: RwLock<Vec<Arc<MemCgroup>>>,
    
    /// Associated CSS
    css: Weak<CgroupSubsysState>,
    
    /// OOM control settings
    oom_control: SpinLock<OomControl>,
}

/// OOM (Out of Memory) control settings
#[derive(Debug, Clone)]
pub struct OomControl {
    /// Whether OOM killer is disabled for this cgroup
    pub oom_kill_disable: bool,
    
    /// Whether this cgroup is under OOM condition
    pub under_oom: bool,
    
    /// OOM score adjustment
    pub oom_score_adj: i32,
}

impl Default for OomControl {
    fn default() -> Self {
        Self {
            oom_kill_disable: false,
            under_oom: false,
            oom_score_adj: 0,
        }
    }
}

impl MemCgroup {
    /// Create a new memory cgroup
    pub fn new(parent: Option<Arc<MemCgroup>>, css: Weak<CgroupSubsysState>) -> Arc<Self> {
        let mem_cgroup = Arc::new(Self {
            stats: MemCgroupStats::new(),
            parent: RwLock::new(parent.clone()),
            children: RwLock::new(Vec::new()),
            css,
            oom_control: SpinLock::new(OomControl::default()),
        });
        
        // Add to parent's children list
        if let Some(parent) = parent {
            parent.children.write().push(mem_cgroup.clone());
        }
        
        mem_cgroup
    }
    
    /// Get memory statistics
    pub fn stats(&self) -> &MemCgroupStats {
        &self.stats
    }
    
    /// Get parent memory cgroup
    pub fn parent(&self) -> Option<Arc<MemCgroup>> {
        self.parent.read().clone()
    }
    
    /// Get children memory cgroups
    pub fn children(&self) -> RwLockReadGuard<'_, Vec<Arc<MemCgroup>>> {
        self.children.read()
    }
    
    /// Add child memory cgroup
    pub fn add_child(&self, child: Arc<MemCgroup>) {
        self.children.write().push(child);
    }
    
    /// Remove child memory cgroup
    pub fn remove_child(&self, child: &Arc<MemCgroup>) {
        self.children.write().retain(|c| !Arc::ptr_eq(c, child));
    }
    
    /// Charge memory to this cgroup and its ancestors
    pub fn charge(&self, bytes: u64) -> Result<(), SystemError> {
        // Charge current cgroup
        self.stats.add_usage(bytes)?;
        
        // Charge ancestors
        let mut current = self.parent();
        while let Some(parent) = current {
            parent.stats.add_usage(bytes)?;
            current = parent.parent();
        }
        
        Ok(())
    }
    
    /// Uncharge memory from this cgroup and its ancestors
    pub fn uncharge(&self, bytes: u64) {
        // Uncharge current cgroup
        self.stats.sub_usage(bytes);
        
        // Uncharge ancestors
        let mut current = self.parent();
        while let Some(parent) = current {
            parent.stats.sub_usage(bytes);
            current = parent.parent();
        }
    }
    
    /// Charge anonymous memory
    pub fn charge_anon(&self, bytes: u64) -> Result<(), SystemError> {
        self.charge(bytes)?;
        self.stats.add_anon_usage(bytes);
        Ok(())
    }
    
    /// Uncharge anonymous memory
    pub fn uncharge_anon(&self, bytes: u64) {
        self.uncharge(bytes);
        self.stats.sub_anon_usage(bytes);
    }
    
    /// Charge file memory
    pub fn charge_file(&self, bytes: u64) -> Result<(), SystemError> {
        self.charge(bytes)?;
        self.stats.add_file_usage(bytes);
        Ok(())
    }
    
    /// Uncharge file memory
    pub fn uncharge_file(&self, bytes: u64) {
        self.uncharge(bytes);
        self.stats.sub_file_usage(bytes);
    }
    
    /// Charge kernel memory
    pub fn charge_kernel(&self, bytes: u64) -> Result<(), SystemError> {
        self.charge(bytes)?;
        self.stats.add_kernel_usage(bytes);
        Ok(())
    }
    
    /// Uncharge kernel memory
    pub fn uncharge_kernel(&self, bytes: u64) {
        self.uncharge(bytes);
        self.stats.sub_kernel_usage(bytes);
    }
    
    /// Check if memory usage exceeds limit
    pub fn over_limit(&self) -> bool {
        self.stats.usage() > self.stats.limit()
    }
    
    /// Check if memory usage exceeds soft limit
    pub fn over_soft_limit(&self) -> bool {
        self.stats.usage() > self.stats.soft_limit()
    }
    
    /// Try to reclaim memory
    pub fn try_reclaim(&self, _bytes: u64) -> Result<u64, SystemError> {
        // TODO: Implement memory reclaim logic
        // This would involve:
        // 1. Scanning pages in this cgroup
        // 2. Reclaiming clean file pages
        // 3. Swapping out anonymous pages
        // 4. Shrinking caches
        
        // For now, return 0 (no memory reclaimed)
        Ok(0)
    }
    
    /// Handle OOM condition
    pub fn handle_oom(&self) -> Result<(), SystemError> {
        let mut oom_control = self.oom_control.lock();
        
        if oom_control.oom_kill_disable {
            // OOM killer is disabled, just mark as under OOM
            oom_control.under_oom = true;
            return Err(SystemError::ENOMEM);
        }
        
        // TODO: Implement OOM killer logic
        // This would involve:
        // 1. Finding the best process to kill
        // 2. Sending SIGKILL to the process
        // 3. Updating statistics
        
        self.stats.inc_oom_kills();
        oom_control.under_oom = false;
        
        Ok(())
    }
    
    /// Get OOM control settings
    pub fn oom_control(&self) -> SpinLockGuard<'_, OomControl> {
        self.oom_control.lock()
    }
    
    /// Set memory limit
    pub fn set_limit(&self, limit: u64) -> Result<(), SystemError> {
        // Check if new limit is valid
        if limit < self.stats.usage() {
            // Try to reclaim memory first
            let need_reclaim = self.stats.usage() - limit;
            let reclaimed = self.try_reclaim(need_reclaim)?;
            
            if reclaimed < need_reclaim {
                return Err(SystemError::EBUSY);
            }
        }
        
        self.stats.set_limit(limit);
        Ok(())
    }
    
    /// Set soft memory limit
    pub fn set_soft_limit(&self, limit: u64) -> Result<(), SystemError> {
        self.stats.set_soft_limit(limit);
        Ok(())
    }
    
    /// Reset memory statistics
    pub fn reset_stats(&self) {
        self.stats.max_usage.store(self.stats.usage(), Ordering::SeqCst);
        self.stats.page_faults.store(0, Ordering::SeqCst);
        self.stats.major_page_faults.store(0, Ordering::SeqCst);
        self.stats.oom_kills.store(0, Ordering::SeqCst);
    }
}

/// Memory cgroup subsystem implementation
#[derive(Debug)]
pub struct MemCgroupSubsystem;

impl MemCgroupSubsystem {
    /// Create a new memory cgroup subsystem
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
    
    /// Get memory cgroup from CSS
    pub fn mem_cgroup_from_css(_css: &Arc<CgroupSubsysState>) -> Option<Arc<MemCgroup>> {
        // In a real implementation, this would extract the MemCgroup from the CSS
        // For now, we'll use a placeholder
        None
    }
    
    /// Read memory.current file
    fn read_memory_current(css: &Arc<CgroupSubsysState>) -> Result<String, SystemError> {
        if let Some(mem_cgroup) = Self::mem_cgroup_from_css(css) {
            Ok(mem_cgroup.stats().usage().to_string())
        } else {
            Ok("0".to_string())
        }
    }
    
    /// Read memory.max file
    fn read_memory_max(css: &Arc<CgroupSubsysState>) -> Result<String, SystemError> {
        if let Some(mem_cgroup) = Self::mem_cgroup_from_css(css) {
            let limit = mem_cgroup.stats().limit();
            if limit == u64::MAX {
                Ok("max".to_string())
            } else {
                Ok(limit.to_string())
            }
        } else {
            Ok("max".to_string())
        }
    }
    
    /// Write memory.max file
    fn write_memory_max(css: &Arc<CgroupSubsysState>, data: &str) -> Result<(), SystemError> {
        let limit = if data.trim() == "max" {
            u64::MAX
        } else {
            data.trim().parse::<u64>().map_err(|_| SystemError::EINVAL)?
        };
        
        if let Some(mem_cgroup) = Self::mem_cgroup_from_css(css) {
            mem_cgroup.set_limit(limit)?;
        }
        
        Ok(())
    }
    
    /// Read memory.stat file
    fn read_memory_stat(css: &Arc<CgroupSubsysState>) -> Result<String, SystemError> {
        if let Some(mem_cgroup) = Self::mem_cgroup_from_css(css) {
            let stats = mem_cgroup.stats();
            Ok(format!(
                "anon {}\n\
                 file {}\n\
                 kernel {}\n\
                 kernel_stack 0\n\
                 pagetables 0\n\
                 percpu 0\n\
                 sock 0\n\
                 shmem 0\n\
                 file_mapped 0\n\
                 file_dirty 0\n\
                 file_writeback 0\n\
                 swapcached 0\n\
                 anon_thp 0\n\
                 file_thp 0\n\
                 shmem_thp 0\n\
                 inactive_anon 0\n\
                 active_anon {}\n\
                 inactive_file 0\n\
                 active_file {}\n\
                 unevictable 0\n\
                 slab_reclaimable 0\n\
                 slab_unreclaimable 0\n\
                 slab 0\n\
                 workingset_refault_anon 0\n\
                 workingset_refault_file 0\n\
                 workingset_activate_anon 0\n\
                 workingset_activate_file 0\n\
                 workingset_restore_anon 0\n\
                 workingset_restore_file 0\n\
                 workingset_nodereclaim 0\n\
                 pgfault {}\n\
                 pgmajfault {}\n\
                 pgrefill 0\n\
                 pgscan 0\n\
                 pgsteal 0\n\
                 pgactivate 0\n\
                 pgdeactivate 0\n\
                 pglazyfree 0\n\
                 pglazyfreed 0\n\
                 thp_fault_alloc 0\n\
                 thp_collapse_alloc 0\n",
                stats.anon_usage.load(Ordering::SeqCst),
                stats.file_usage.load(Ordering::SeqCst),
                stats.kernel_usage.load(Ordering::SeqCst),
                stats.anon_usage.load(Ordering::SeqCst),
                stats.file_usage.load(Ordering::SeqCst),
                stats.page_faults.load(Ordering::SeqCst),
                stats.major_page_faults.load(Ordering::SeqCst),
            ))
        } else {
            Ok("anon 0\nfile 0\nkernel 0\n".to_string())
        }
    }
    
    /// Read memory.events file
    fn read_memory_events(css: &Arc<CgroupSubsysState>) -> Result<String, SystemError> {
        if let Some(mem_cgroup) = Self::mem_cgroup_from_css(css) {
            let stats = mem_cgroup.stats();
            Ok(format!(
                "low 0\n\
                 high 0\n\
                 max 0\n\
                 oom {}\n\
                 oom_kill {}\n\
                 oom_group_kill 0\n",
                stats.oom_kills.load(Ordering::SeqCst),
                stats.oom_kills.load(Ordering::SeqCst),
            ))
        } else {
            Ok("low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\n".to_string())
        }
    }
    
    /// Read memory.peak file
    fn read_memory_peak(css: &Arc<CgroupSubsysState>) -> Result<String, SystemError> {
        if let Some(mem_cgroup) = Self::mem_cgroup_from_css(css) {
            Ok(mem_cgroup.stats().max_usage.load(Ordering::SeqCst).to_string())
        } else {
            Ok("0".to_string())
        }
    }
    
    /// Write memory.peak file (reset peak usage)
    fn write_memory_peak(css: &Arc<CgroupSubsysState>, _data: &str) -> Result<(), SystemError> {
        if let Some(mem_cgroup) = Self::mem_cgroup_from_css(css) {
            mem_cgroup.reset_stats();
        }
        Ok(())
    }
}

impl CgroupSubsystem for MemCgroupSubsystem {
    fn name(&self) -> &'static str {
        "memory"
    }
    
    fn id(&self) -> CgroupSubsysId {
        MEM_CGROUP_SUBSYS_ID
    }
    
    fn css_alloc(&self, parent: Option<&Arc<CgroupSubsysState>>) -> Result<Arc<CgroupSubsysState>, SystemError> {
        // Get parent memory cgroup
        let parent_mem_cgroup = parent
            .and_then(|p| Self::mem_cgroup_from_css(p));
        
        // Create new CSS
        let css = CgroupSubsysState::new(
            Some(Arc::new(Self) as Arc<dyn CgroupSubsystem>),
            Weak::new(), // Will be set later
            parent.cloned(),
            self.id(),
        );
        
        // Create memory cgroup
        let _mem_cgroup = MemCgroup::new(parent_mem_cgroup, Arc::downgrade(&css));
        
        // TODO: Associate mem_cgroup with css
        // In a real implementation, we would store the MemCgroup in the CSS
        
        Ok(css)
    }
    
    fn css_free(&self, _css: &Arc<CgroupSubsysState>) -> Result<(), SystemError> {
        // TODO: Free memory cgroup resources
        Ok(())
    }
    
    fn attach(&self, _css: &Arc<CgroupSubsysState>, _pid: RawPid) -> Result<(), SystemError> {
        // TODO: Move task's memory accounting to this cgroup
        // This would involve updating page ownership and statistics
        Ok(())
    }
    
    fn detach(&self, _css: &Arc<CgroupSubsysState>, _pid: RawPid) -> Result<(), SystemError> {
        // TODO: Remove task's memory accounting from this cgroup
        Ok(())
    }
    
    fn fork(&self, css: &Arc<CgroupSubsysState>, pid: RawPid) -> Result<(), SystemError> {
        // Child inherits parent's memory cgroup
        self.attach(css, pid)
    }
    
    fn exit(&self, css: &Arc<CgroupSubsysState>, pid: RawPid) -> Result<(), SystemError> {
        // Remove task from memory cgroup
        self.detach(css, pid)
    }
    
    fn files(&self) -> Vec<CfType> {
        vec![
            CfType {
                name: "memory.current".to_string(),
                flags: 0,
                read: Some(Self::read_memory_current),
                write: None,
                subsys: Some(self.id()),
            },
            CfType {
                name: "memory.max".to_string(),
                flags: 0,
                read: Some(Self::read_memory_max),
                write: Some(Self::write_memory_max),
                subsys: Some(self.id()),
            },
            CfType {
                name: "memory.stat".to_string(),
                flags: 0,
                read: Some(Self::read_memory_stat),
                write: None,
                subsys: Some(self.id()),
            },
            CfType {
                name: "memory.events".to_string(),
                flags: 0,
                read: Some(Self::read_memory_events),
                write: None,
                subsys: Some(self.id()),
            },
            CfType {
                name: "memory.peak".to_string(),
                flags: 0,
                read: Some(Self::read_memory_peak),
                write: Some(Self::write_memory_peak),
                subsys: Some(self.id()),
            },
        ]
    }
    
    fn can_disable(&self) -> bool {
        false // Memory cgroup cannot be disabled
    }
}

/// Initialize memory cgroup subsystem
pub fn mem_cgroup_init() -> Result<(), SystemError> {
    log::info!("Getting cgroup manager for memory subsystem...");
    let manager = cgroup_manager().ok_or_else(|| {
        log::error!("Failed to get cgroup manager - it's None!");
        SystemError::ENOSYS
    })?;
    log::info!("Got cgroup manager successfully");
    
    log::info!("Creating memory cgroup subsystem...");
    let mem_subsys = MemCgroupSubsystem::new();
    log::info!("Registering memory cgroup subsystem...");
    manager.register_subsystem(mem_subsys as Arc<dyn CgroupSubsystem>)?;
    log::info!("Memory cgroup subsystem registered successfully");
    Ok(())
}

/// Get current task's memory cgroup
pub fn current_mem_cgroup() -> Option<Arc<MemCgroup>> {
    let manager = cgroup_manager()?;
    let current_pid = crate::process::ProcessManager::current_pid();
    let css_set = manager.task_css_set(current_pid)?;
    let css = css_set.css(MEM_CGROUP_SUBSYS_ID)?;
    MemCgroupSubsystem::mem_cgroup_from_css(&css)
}

/// Charge memory to current task's memory cgroup
pub fn mem_cgroup_charge(bytes: u64) -> Result<(), SystemError> {
    if let Some(mem_cgroup) = current_mem_cgroup() {
        mem_cgroup.charge(bytes)?;
    }
    Ok(())
}

/// Uncharge memory from current task's memory cgroup
pub fn mem_cgroup_uncharge(bytes: u64) {
    if let Some(mem_cgroup) = current_mem_cgroup() {
        mem_cgroup.uncharge(bytes);
    }
}

/// Charge anonymous memory to current task's memory cgroup
pub fn mem_cgroup_charge_anon(bytes: u64) -> Result<(), SystemError> {
    if let Some(mem_cgroup) = current_mem_cgroup() {
        mem_cgroup.charge_anon(bytes)?;
    }
    Ok(())
}

/// Uncharge anonymous memory from current task's memory cgroup
pub fn mem_cgroup_uncharge_anon(bytes: u64) {
    if let Some(mem_cgroup) = current_mem_cgroup() {
        mem_cgroup.uncharge_anon(bytes);
    }
}

/// Charge file memory to current task's memory cgroup
pub fn mem_cgroup_charge_file(bytes: u64) -> Result<(), SystemError> {
    if let Some(mem_cgroup) = current_mem_cgroup() {
        mem_cgroup.charge_file(bytes)?;
    }
    Ok(())
}

/// Uncharge file memory from current task's memory cgroup
pub fn mem_cgroup_uncharge_file(bytes: u64) {
    if let Some(mem_cgroup) = current_mem_cgroup() {
        mem_cgroup.uncharge_file(bytes);
    }
}

/// Charge kernel memory to current task's memory cgroup
pub fn mem_cgroup_charge_kernel(bytes: u64) -> Result<(), SystemError> {
    if let Some(mem_cgroup) = current_mem_cgroup() {
        mem_cgroup.charge_kernel(bytes)?;
    }
    Ok(())
}

/// Uncharge kernel memory from current task's memory cgroup
pub fn mem_cgroup_uncharge_kernel(bytes: u64) {
    if let Some(mem_cgroup) = current_mem_cgroup() {
        mem_cgroup.uncharge_kernel(bytes);
    }
}

/// Handle page fault for memory cgroup accounting
pub fn mem_cgroup_handle_page_fault(is_major: bool) {
    if let Some(mem_cgroup) = current_mem_cgroup() {
        mem_cgroup.stats().inc_page_faults();
        if is_major {
            mem_cgroup.stats().inc_major_page_faults();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::sync::Arc;

    #[test]
    fn test_mem_cgroup_stats() {
        let stats = MemCgroupStats::new();
        
        // Test initial values
        assert_eq!(stats.usage(), 0);
        assert_eq!(stats.limit(), u64::MAX);
        
        // Test usage tracking
        assert!(stats.add_usage(1024).is_ok());
        assert_eq!(stats.usage(), 1024);
        
        stats.sub_usage(512);
        assert_eq!(stats.usage(), 512);
        
        // Test limit enforcement
        stats.set_limit(1000);
        assert!(stats.add_usage(600).is_err()); // Would exceed limit
        assert_eq!(stats.usage(), 512); // Usage should not change
    }
    
    #[test]
    fn test_mem_cgroup_hierarchy() {
        let css = CgroupSubsysState::new(None, Weak::new(), None, 0);
        let parent = MemCgroup::new(None, Arc::downgrade(&css));
        let child = MemCgroup::new(Some(parent.clone()), Arc::downgrade(&css));
        
        // Test hierarchy
        assert!(child.parent().is_some());
        assert_eq!(parent.children().len(), 1);
        
        // Test hierarchical charging
        assert!(child.charge(1024).is_ok());
        assert_eq!(child.stats().usage(), 1024);
        assert_eq!(parent.stats().usage(), 1024); // Parent should also be charged
    }
    
    #[test]
    fn test_mem_cgroup_limits() {
        let css = CgroupSubsysState::new(None, Weak::new(), None, 0);
        let mem_cgroup = MemCgroup::new(None, Arc::downgrade(&css));
        
        // Set limit
        assert!(mem_cgroup.set_limit(2048).is_ok());
        assert_eq!(mem_cgroup.stats().limit(), 2048);
        
        // Test charging within limit
        assert!(mem_cgroup.charge(1024).is_ok());
        assert_eq!(mem_cgroup.stats().usage(), 1024);
        
        // Test charging beyond limit
        assert!(mem_cgroup.charge(1500).is_err());
        assert_eq!(mem_cgroup.stats().usage(), 1024); // Should not change
    }
}