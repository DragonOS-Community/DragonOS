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

/// Early cgroup initialization (similar to Linux cgroup_init_early)
/// 
/// This should be called early in kernel initialization, after memory management
/// but before most other subsystems. It sets up the basic cgroup infrastructure.
pub fn cgroup_init_early() -> Result<(), SystemError> {
    log::info!("Early cgroup initialization - setting up core infrastructure");
    
    // Initialize core cgroup infrastructure (manager, root cgroup)
    super::cgroup_core_init()?;
    
    log::info!("Cgroup core infrastructure initialized successfully");
    Ok(())
}

/// Main cgroup initialization (similar to Linux cgroup_init)
/// 
/// This should be called after VFS initialization. It registers subsystems
/// and sets up the cgroup filesystem infrastructure.
pub fn cgroup_init() -> Result<(), SystemError> {
    log::info!("Main cgroup initialization - registering subsystems");
    
    // Initialize cgroup filesystem infrastructure
    cgroup_fs_init()?;
    
    // Initialize memory cgroup subsystem
    mem_cgroup_init()?;
    
    // Initialize CPU cgroup subsystem
    cpu_cgroup_init()?;
    
    // TODO: Mount cgroup filesystem at /sys/fs/cgroup
    // 暂时禁用挂载以避免与其他子系统的冲突
    // mount_cgroup_current_ns()?;
    log::info!("Cgroup filesystem mount deferred until VFS integration is complete");
    
    log::info!("Cgroup subsystem initialized successfully");
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
        assert!(cgroup_init_early().is_ok());
        assert!(cgroup_init().is_ok());
    }
}