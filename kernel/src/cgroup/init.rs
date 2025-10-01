#![allow(dead_code)]
//! Cgroup Initialization Module
//! 
//! This module handles the initialization of the cgroup subsystem and
//! all registered cgroup controllers.

use system_error::SystemError;

use super::{
    mem_cgroup::mem_cgroup_init,
    cpu_cgroup::cpu_cgroup_init,
    cgroup_fs::{cgroup_fs_init, mount_cgroup_current_ns},
};

/// Initialize all cgroup subsystems
/// 
/// This function should be called during kernel initialization to set up
/// the cgroup v2 infrastructure and register all built-in subsystems.
pub fn init_cgroups() -> Result<(), SystemError> {
    log::info!("Initializing cgroup subsystem");
    
    // Initialize core cgroup infrastructure
    cgroup_fs_init()?;
    
    // Initialize memory cgroup subsystem
    mem_cgroup_init()?;
    
    // Initialize CPU cgroup subsystem
    cpu_cgroup_init()?;
    
    // TODO: Mount cgroup filesystem at /sys/fs/cgroup
    // 暂时禁用挂载以避免与其他子系统的冲突
    // mount_cgroup_current_ns()?;
    log::info!("Cgroup filesystem mount deferred");
    
    log::info!("Cgroup v2 subsystem initialized successfully");
    
    Ok(())
}

/// Cgroup subsystem initialization order
/// 
/// Some subsystems may depend on others, so we need to initialize them
/// in the correct order. Currently:
/// 1. Core cgroup infrastructure (manager, root cgroup)
/// 2. Memory cgroup subsystem (fundamental resource)
/// 3. CPU cgroup subsystem (scheduling resource)
/// 4. Mount cgroup filesystem at /sys/fs/cgroup
/// 5. Other subsystems as needed
pub const CGROUP_INIT_ORDER: &[&str] = &[
    "cgroup_core",      // Core infrastructure
    "memory",           // Memory controller
    "cpu",              // CPU controller
    "filesystem",       // VFS integration
    // Add other subsystems here
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cgroup_init() {
        // Test that cgroup initialization doesn't panic
        // In a real kernel environment, this would verify proper setup
        assert!(init_cgroups().is_ok());
    }
}