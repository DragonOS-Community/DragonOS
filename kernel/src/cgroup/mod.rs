#![allow(dead_code)]
//! Cgroup v2 Implementation for DragonOS
//! 
//! This module implements cgroup v2 (Control Groups version 2) for DragonOS,
//! following the Linux kernel design patterns. It provides a unified hierarchy
//! for resource management and process organization.

use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use hashbrown::HashMap;
use crate::process::RawPid;
use core::{
    any::Any,
    fmt,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};
use system_error::SystemError;

use crate::{
    filesystem::{
        vfs::{FileSystem, FsInfo, IndexNode},
    },
    libs::{
        rwlock::{RwLock, RwLockReadGuard},
        spinlock::SpinLock,
    },
};

// Re-export important types
// Standard Linux-style function names
pub use init::{cgroup_init_early, cgroup_init};

pub mod mem_cgroup;
pub mod cpu_cgroup;
pub mod init;
pub mod cgroup_fs;

/// Cgroup subsystem ID type
pub type CgroupSubsysId = u32;

/// Cgroup ID type  
pub type CgroupId = u64;

/// CSS serial number type
pub type CssSerialNr = u64;

/// Maximum number of cgroup subsystems
pub const CGROUP_SUBSYS_COUNT: usize = 16;

/// Global cgroup ID counter
static CGROUP_ID_NEXT: AtomicU64 = AtomicU64::new(1);

/// Global CSS serial number counter
static CSS_SERIAL_NR_NEXT: AtomicU64 = AtomicU64::new(1);

/// Cgroup flags
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CgroupFlags(u32);

impl CgroupFlags {
    pub const NONE: Self = Self(0);
    pub const POPULATED: Self = Self(1 << 0);
    pub const FROZEN: Self = Self(1 << 1);
    pub const KILL: Self = Self(1 << 2);
    pub const THREADED: Self = Self(1 << 3);
    
    pub fn contains(&self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
    
    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }
    
    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }
}

/// CSS flags
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CssFlags(u32);

impl CssFlags {
    pub const NONE: Self = Self(0);
    pub const ONLINE: Self = Self(1 << 0);
    pub const RELEASED: Self = Self(1 << 1);
    pub const VISIBLE: Self = Self(1 << 2);
    pub const DYING: Self = Self(1 << 3);
    
    pub fn contains(&self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
    
    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }
    
    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }
}

/// Cgroup subsystem state
/// 
/// This represents the state of a cgroup in a particular subsystem.
/// Each cgroup has one CSS per enabled subsystem.
#[derive(Debug)]
pub struct CgroupSubsysState {
    /// Subsystem this CSS belongs to
    subsystem: Option<Arc<dyn CgroupSubsystem>>,
    
    /// The cgroup this CSS is attached to
    cgroup: Weak<Cgroup>,
    
    /// Parent CSS
    parent: Option<Arc<CgroupSubsysState>>,
    
    /// Children CSS list
    children: RwLock<Vec<Arc<CgroupSubsysState>>>,
    
    /// CSS ID within the subsystem
    id: CgroupSubsysId,
    
    /// CSS serial number for ordering
    serial_nr: CssSerialNr,
    
    /// CSS flags
    flags: SpinLock<CssFlags>,
    
    /// Reference count
    ref_count: AtomicUsize,
}

impl CgroupSubsysState {
    /// Create a new CSS
    pub fn new(
        subsystem: Option<Arc<dyn CgroupSubsystem>>,
        cgroup: Weak<Cgroup>,
        parent: Option<Arc<CgroupSubsysState>>,
        id: CgroupSubsysId,
    ) -> Arc<Self> {
        Arc::new(Self {
            subsystem,
            cgroup,
            parent,
            children: RwLock::new(Vec::new()),
            id,
            serial_nr: CSS_SERIAL_NR_NEXT.fetch_add(1, Ordering::SeqCst),
            flags: SpinLock::new(CssFlags::NONE),
            ref_count: AtomicUsize::new(1),
        })
    }
    
    /// Get CSS ID
    pub fn id(&self) -> CgroupSubsysId {
        self.id
    }
    
    /// Get CSS serial number
    pub fn serial_nr(&self) -> CssSerialNr {
        self.serial_nr
    }
    
    /// Check if CSS is online
    pub fn is_online(&self) -> bool {
        self.flags.lock().contains(CssFlags::ONLINE)
    }
    
    /// Set CSS online
    pub fn set_online(&self) {
        self.flags.lock().insert(CssFlags::ONLINE);
    }
    
    /// Set CSS offline
    pub fn set_offline(&self) {
        self.flags.lock().remove(CssFlags::ONLINE);
    }
    
    /// Get parent CSS
    pub fn parent(&self) -> Option<Arc<CgroupSubsysState>> {
        self.parent.clone()
    }
    
    /// Get children CSS
    pub fn children(&self) -> RwLockReadGuard<'_, Vec<Arc<CgroupSubsysState>>> {
        self.children.read()
    }
    
    /// Add child CSS
    pub fn add_child(&self, child: Arc<CgroupSubsysState>) {
        self.children.write().push(child);
    }
    
    /// Remove child CSS
    pub fn remove_child(&self, child: &Arc<CgroupSubsysState>) {
        self.children.write().retain(|c| !Arc::ptr_eq(c, child));
    }
}

/// CSS set - collection of CSS pointers for a task
/// 
/// Each task is associated with one css_set, which contains pointers
/// to the CSS for each subsystem.
#[derive(Debug)]
pub struct CssSet {
    /// CSS pointers for each subsystem
    subsys: RwLock<[Option<Arc<CgroupSubsysState>>; CGROUP_SUBSYS_COUNT]>,
    
    /// Tasks using this css_set
    tasks: RwLock<Vec<RawPid>>,
    
    /// Reference count
    ref_count: AtomicUsize,
    
    /// Default cgroup (for the unified hierarchy)
    dfl_cgrp: RwLock<Weak<Cgroup>>,
}

impl CssSet {
    /// Create a new CSS set
    pub fn new() -> Arc<Self> {
        const INIT: Option<Arc<CgroupSubsysState>> = None;
        Arc::new(Self {
            subsys: RwLock::new([INIT; CGROUP_SUBSYS_COUNT]),
            tasks: RwLock::new(Vec::new()),
            ref_count: AtomicUsize::new(1),
            dfl_cgrp: RwLock::new(Weak::new()),
        })
    }
    
    /// Get CSS for a subsystem
    pub fn css(&self, subsys_id: CgroupSubsysId) -> Option<Arc<CgroupSubsysState>> {
        if (subsys_id as usize) >= CGROUP_SUBSYS_COUNT {
            return None;
        }
        self.subsys.read()[subsys_id as usize].clone()
    }
    
    /// Set CSS for a subsystem
    pub fn set_css(&self, subsys_id: CgroupSubsysId, css: Option<Arc<CgroupSubsysState>>) {
        if (subsys_id as usize) < CGROUP_SUBSYS_COUNT {
            self.subsys.write()[subsys_id as usize] = css;
        }
    }
    
    /// Add task to this css_set
    pub fn add_task(&self, pid: RawPid) {
        self.tasks.write().push(pid);
    }
    
    /// Remove task from this css_set
    pub fn remove_task(&self, pid: RawPid) {
        self.tasks.write().retain(|p| *p != pid);
    }
    
    /// Get tasks in this css_set
    pub fn tasks(&self) -> RwLockReadGuard<'_, Vec<RawPid>> {
        self.tasks.read()
    }
    
    /// Get default cgroup
    pub fn default_cgroup(&self) -> Option<Arc<Cgroup>> {
        self.dfl_cgrp.read().upgrade()
    }
    
    /// Set default cgroup
    pub fn set_default_cgroup(&self, cgroup: Weak<Cgroup>) {
        *self.dfl_cgrp.write() = cgroup;
    }
}

/// Cgroup - represents a control group
/// 
/// A cgroup is a collection of processes that are bound by the same
/// resource constraints and are subject to the same resource limits.
#[derive(Debug)]
pub struct Cgroup {
    /// Cgroup ID
    id: CgroupId,
    
    /// Cgroup level in the hierarchy (root = 0)
    level: u32,
    
    /// Cgroup flags
    flags: SpinLock<CgroupFlags>,
    
    /// Parent cgroup
    parent: RwLock<Weak<Cgroup>>,
    
    /// Children cgroups
    children: RwLock<Vec<Arc<Cgroup>>>,
    
    /// Root of this cgroup hierarchy
    root: Weak<CgroupRoot>,
    
    /// CSS for each subsystem
    subsys: RwLock<[Option<Arc<CgroupSubsysState>>; CGROUP_SUBSYS_COUNT]>,
    
    /// Cgroup name
    name: String,
    
    /// Enabled subsystems mask
    subtree_control: AtomicU64,
    
    /// Tasks in this cgroup
    tasks: RwLock<Vec<RawPid>>,
    
    /// Number of populated descendant cgroups
    nr_populated_csets: AtomicUsize,
    
    /// Number of descendant cgroups
    nr_descendants: AtomicUsize,
    
    /// Maximum allowed descendants
    max_descendants: AtomicUsize,
    
    /// Maximum allowed depth
    max_depth: AtomicUsize,
}

impl Cgroup {
    /// Create a new cgroup
    pub fn new(
        name: String,
        parent: Option<Arc<Cgroup>>,
        root: Weak<CgroupRoot>,
    ) -> Result<Arc<Self>, SystemError> {
        let level = parent.as_ref().map_or(0, |p| p.level + 1);
        let id = CGROUP_ID_NEXT.fetch_add(1, Ordering::SeqCst);
        
        const INIT: Option<Arc<CgroupSubsysState>> = None;
        let cgroup = Arc::new(Self {
            id,
            level,
            flags: SpinLock::new(CgroupFlags::NONE),
            parent: RwLock::new(parent.as_ref().map(|p| Arc::downgrade(p)).unwrap_or_else(Weak::new)),
            children: RwLock::new(Vec::new()),
            root,
            subsys: RwLock::new([INIT; CGROUP_SUBSYS_COUNT]),
            name,
            subtree_control: AtomicU64::new(0),
            tasks: RwLock::new(Vec::new()),
            nr_populated_csets: AtomicUsize::new(0),
            nr_descendants: AtomicUsize::new(0),
            max_descendants: AtomicUsize::new(usize::MAX),
            max_depth: AtomicUsize::new(usize::MAX),
        });
        
        // Add to parent's children list
        if let Some(parent) = parent {
            parent.children.write().push(cgroup.clone());
            parent.nr_descendants.fetch_add(1, Ordering::SeqCst);
        }
        
        Ok(cgroup)
    }
    
    /// Get cgroup ID
    pub fn id(&self) -> CgroupId {
        self.id
    }
    
    /// Get cgroup level
    pub fn level(&self) -> u32 {
        self.level
    }
    
    /// Get cgroup name
    pub fn name(&self) -> &str {
        &self.name
    }
    
    /// Get parent cgroup
    pub fn parent(&self) -> Option<Arc<Cgroup>> {
        self.parent.read().upgrade()
    }
    
    /// Get children cgroups
    pub fn children(&self) -> RwLockReadGuard<'_, Vec<Arc<Cgroup>>> {
        self.children.read()
    }
    
    /// Add child cgroup
    pub fn add_child(&self, child: Arc<Cgroup>) {
        self.children.write().push(child);
        self.nr_descendants.fetch_add(1, Ordering::SeqCst);
    }
    
    /// Remove child cgroup
    pub fn remove_child(&self, child: &Arc<Cgroup>) {
        self.children.write().retain(|c| !Arc::ptr_eq(c, child));
        self.nr_descendants.fetch_sub(1, Ordering::SeqCst);
    }
    
    /// Get CSS for a subsystem
    pub fn css(&self, subsys_id: CgroupSubsysId) -> Option<Arc<CgroupSubsysState>> {
        if subsys_id as usize >= CGROUP_SUBSYS_COUNT {
            return None;
        }
        self.subsys.read()[subsys_id as usize].clone()
    }
    
    /// Set CSS for a subsystem
    pub fn set_css(&self, subsys_id: CgroupSubsysId, css: Option<Arc<CgroupSubsysState>>) {
        if (subsys_id as usize) < CGROUP_SUBSYS_COUNT {
            self.subsys.write()[subsys_id as usize] = css;
        }
    }
    
    /// Add task to this cgroup
    pub fn add_task(&self, pid: RawPid) {
        self.tasks.write().push(pid);
    }
    
    /// Remove task from this cgroup
    pub fn remove_task(&self, pid: RawPid) {
        self.tasks.write().retain(|&p| p != pid);
    }
    
    /// Get tasks in this cgroup
    pub fn tasks(&self) -> RwLockReadGuard<'_, Vec<RawPid>> {
        self.tasks.read()
    }
    
    /// Check if cgroup is populated (has tasks or populated descendants)
    pub fn is_populated(&self) -> bool {
        !self.tasks.read().is_empty() || self.nr_populated_csets.load(Ordering::SeqCst) > 0
    }
    
    /// Check if cgroup is root
    pub fn is_root(&self) -> bool {
        self.parent.read().upgrade().is_none()
    }
    
    /// Get subtree control mask
    pub fn subtree_control(&self) -> u64 {
        self.subtree_control.load(Ordering::SeqCst)
    }
    
    /// Set subtree control mask
    pub fn set_subtree_control(&self, mask: u64) {
        self.subtree_control.store(mask, Ordering::SeqCst);
    }
    
    /// Check if a subsystem is enabled
    pub fn subsys_enabled(&self, subsys_id: CgroupSubsysId) -> bool {
        let mask = self.subtree_control();
        (mask & (1u64 << subsys_id)) != 0
    }
    
    /// Enable a subsystem
    pub fn enable_subsys(&self, subsys_id: CgroupSubsysId) {
        let mask = self.subtree_control();
        self.set_subtree_control(mask | (1u64 << subsys_id));
    }
    
    /// Disable a subsystem
    pub fn disable_subsys(&self, subsys_id: CgroupSubsysId) {
        let mask = self.subtree_control();
        self.set_subtree_control(mask & !(1u64 << subsys_id));
    }
}

/// Cgroup root - represents the root of a cgroup hierarchy
#[derive(Debug)]
pub struct CgroupRoot {
    /// Root cgroup
    cgrp: Arc<Cgroup>,
    
    /// Hierarchy ID
    hierarchy_id: u32,
    
    /// Enabled subsystems mask
    subsys_mask: AtomicU64,
    
    /// Root flags
    flags: SpinLock<u32>,
    
    /// Name of this hierarchy
    name: String,
}

impl CgroupRoot {
    /// Create a new cgroup root
    pub fn new(name: String, hierarchy_id: u32) -> Result<Arc<Self>, SystemError> {
        let root = Arc::new_cyclic(|weak_root| {
            let cgrp = Cgroup::new("".to_string(), None, weak_root.clone())
                .expect("Failed to create root cgroup");
            
            Self {
                cgrp,
                hierarchy_id,
                subsys_mask: AtomicU64::new(0),
                flags: SpinLock::new(0),
                name,
            }
        });
        
        Ok(root)
    }
    
    /// Get root cgroup
    pub fn cgroup(&self) -> &Arc<Cgroup> {
        &self.cgrp
    }
    
    /// Get hierarchy ID
    pub fn hierarchy_id(&self) -> u32 {
        self.hierarchy_id
    }
    
    /// Get subsystem mask
    pub fn subsys_mask(&self) -> u64 {
        self.subsys_mask.load(Ordering::SeqCst)
    }
    
    /// Set subsystem mask
    pub fn set_subsys_mask(&self, mask: u64) {
        self.subsys_mask.store(mask, Ordering::SeqCst);
    }
    
    /// Check if a subsystem is enabled
    pub fn subsys_enabled(&self, subsys_id: CgroupSubsysId) -> bool {
        let mask = self.subsys_mask();
        (mask & (1u64 << subsys_id)) != 0
    }
    
    /// Enable a subsystem
    pub fn enable_subsys(&self, subsys_id: CgroupSubsysId) {
        let mask = self.subsys_mask();
        self.set_subsys_mask(mask | (1u64 << subsys_id));
    }
    
    /// Disable a subsystem
    pub fn disable_subsys(&self, subsys_id: CgroupSubsysId) {
        let mask = self.subsys_mask();
        self.set_subsys_mask(mask & !(1u64 << subsys_id));
    }
    
    /// Get name
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Cgroup file type - defines interface files
#[derive(Debug)]
pub struct CfType {
    /// File name
    pub name: String,
    
    /// File flags
    pub flags: u32,
    
    /// Read function
    pub read: Option<fn(&Arc<CgroupSubsysState>) -> Result<String, SystemError>>,
    
    /// Write function
    pub write: Option<fn(&Arc<CgroupSubsysState>, &str) -> Result<(), SystemError>>,
    
    /// Subsystem this file belongs to
    pub subsys: Option<CgroupSubsysId>,
}

/// Cgroup subsystem trait
/// 
/// Each cgroup subsystem (like memory, cpu, etc.) implements this trait
/// to provide subsystem-specific functionality.
pub trait CgroupSubsystem: Send + Sync + fmt::Debug {
    /// Subsystem name
    fn name(&self) -> &'static str;
    
    /// Subsystem ID
    fn id(&self) -> CgroupSubsysId;
    
    /// Create CSS for this subsystem
    fn css_alloc(&self, parent: Option<&Arc<CgroupSubsysState>>) -> Result<Arc<CgroupSubsysState>, SystemError>;
    
    /// Initialize CSS
    fn css_online(&self, css: &Arc<CgroupSubsysState>) -> Result<(), SystemError> {
        css.set_online();
        Ok(())
    }
    
    /// Cleanup CSS
    fn css_offline(&self, css: &Arc<CgroupSubsysState>) {
        css.set_offline();
    }
    
    /// Free CSS
    fn css_free(&self, css: &Arc<CgroupSubsysState>) -> Result<(), SystemError>;
    
    /// Attach task to cgroup
    fn attach(&self, css: &Arc<CgroupSubsysState>, pid: RawPid) -> Result<(), SystemError> {
        // Default implementation - just add to cgroup
        if let Some(cgroup) = css.cgroup.upgrade() {
            cgroup.add_task(pid);
        }
        Ok(())
    }
    
    /// Detach task from cgroup
    fn detach(&self, css: &Arc<CgroupSubsysState>, pid: RawPid) -> Result<(), SystemError> {
        // Default implementation - just remove from cgroup
        if let Some(cgroup) = css.cgroup.upgrade() {
            cgroup.remove_task(pid);
        }
        Ok(())
    }
    
    /// Fork callback
    fn fork(&self, css: &Arc<CgroupSubsysState>, pid: RawPid) -> Result<(), SystemError> {
        // Default implementation - inherit parent's cgroup
        self.attach(css, pid)
    }
    
    /// Exit callback
    fn exit(&self, css: &Arc<CgroupSubsysState>, pid: RawPid) -> Result<(), SystemError> {
        // Default implementation - detach from cgroup
        self.detach(css, pid)
    }
    
    /// Get interface files
    fn files(&self) -> Vec<CfType> {
        Vec::new()
    }
    
    /// Check if subsystem can be disabled
    fn can_disable(&self) -> bool {
        true
    }
}

/// Global cgroup manager
#[derive(Debug)]
pub struct CgroupManager {
    /// Default hierarchy (cgroup v2)
    default_root: Arc<CgroupRoot>,
    
    /// Registered subsystems
    subsystems: RwLock<HashMap<CgroupSubsysId, Arc<dyn CgroupSubsystem>>>,
    
    /// Global css_set hash table
    css_sets: RwLock<HashMap<u64, Arc<CssSet>>>,
    
    /// Task to css_set mapping
    task_css_sets: RwLock<HashMap<RawPid, Arc<CssSet>>>,
}

impl CgroupManager {
    /// Create a new cgroup manager
    pub fn new() -> Result<Arc<Self>, SystemError> {
        let default_root = CgroupRoot::new("cgroup2".to_string(), 1)?;
        
        Ok(Arc::new(Self {
            default_root,
            subsystems: RwLock::new(HashMap::new()),
            css_sets: RwLock::new(HashMap::new()),
            task_css_sets: RwLock::new(HashMap::new()),
        }))
    }
    
    /// Get default root
    pub fn default_root(&self) -> &Arc<CgroupRoot> {
        &self.default_root
    }
    
    /// Register a subsystem
    pub fn register_subsystem(&self, subsys: Arc<dyn CgroupSubsystem>) -> Result<(), SystemError> {
        let id = subsys.id();
        if id as usize >= CGROUP_SUBSYS_COUNT {
            return Err(SystemError::EINVAL);
        }
        
        self.subsystems.write().insert(id, subsys);
        self.default_root.enable_subsys(id);
        
        Ok(())
    }
    
    /// Unregister a subsystem
    pub fn unregister_subsystem(&self, subsys_id: CgroupSubsysId) -> Result<(), SystemError> {
        if let Some(subsys) = self.subsystems.write().remove(&subsys_id) {
            if !subsys.can_disable() {
                // Re-insert if cannot be disabled
                self.subsystems.write().insert(subsys_id, subsys);
                return Err(SystemError::EBUSY);
            }
            self.default_root.disable_subsys(subsys_id);
        }
        Ok(())
    }
    
    /// Get subsystem by ID
    pub fn get_subsystem(&self, subsys_id: CgroupSubsysId) -> Option<Arc<dyn CgroupSubsystem>> {
        self.subsystems.read().get(&subsys_id).cloned()
    }
    
    /// Create a new cgroup
    pub fn create_cgroup(
        &self,
        parent: &Arc<Cgroup>,
        name: String,
    ) -> Result<Arc<Cgroup>, SystemError> {
        let cgroup = Cgroup::new(name, Some(parent.clone()), Arc::downgrade(&self.default_root))?;
        
        // Initialize CSS for enabled subsystems
        let subsys_mask = parent.subtree_control();
        for subsys_id in 0..CGROUP_SUBSYS_COUNT as CgroupSubsysId {
            if (subsys_mask & (1u64 << subsys_id)) != 0 {
                if let Some(subsys) = self.get_subsystem(subsys_id) {
                    let parent_css = parent.css(subsys_id);
                    let css = subsys.css_alloc(parent_css.as_ref())?;
                    cgroup.set_css(subsys_id, Some(css.clone()));
                    subsys.css_online(&css)?;
                }
            }
        }
        
        Ok(cgroup)
    }
    
    /// Remove a cgroup
    pub fn remove_cgroup(&self, cgroup: &Arc<Cgroup>) -> Result<(), SystemError> {
        // Check if cgroup is empty
        if cgroup.is_populated() {
            return Err(SystemError::EBUSY);
        }
        
        // Check if cgroup has children
        if !cgroup.children().is_empty() {
            return Err(SystemError::EBUSY);
        }
        
        // Offline CSS for all subsystems
        for subsys_id in 0..CGROUP_SUBSYS_COUNT as CgroupSubsysId {
            if let Some(css) = cgroup.css(subsys_id) {
                if let Some(subsys) = self.get_subsystem(subsys_id) {
                    subsys.css_offline(&css);
                }
            }
        }
        
        // Remove from parent
        if let Some(parent) = cgroup.parent() {
            parent.remove_child(cgroup);
        }
        
        Ok(())
    }
    
    /// Get task's css_set
    pub fn task_css_set(&self, pid: RawPid) -> Option<Arc<CssSet>> {
        self.task_css_sets.read().get(&pid).cloned()
    }
    
    /// Get task's cgroup for a subsystem
    pub fn task_cgroup(&self, pid: RawPid, subsys_id: CgroupSubsysId) -> Option<Arc<Cgroup>> {
        let css_set = self.task_css_set(pid)?;
        let css = css_set.css(subsys_id)?;
        css.cgroup.upgrade()
    }
    
    /// Find or create css_set for a cgroup
    fn find_or_create_css_set(&self, cgroup: &Arc<Cgroup>) -> Result<Arc<CssSet>, SystemError> {
        // Calculate hash based on CSS pointers
        let mut hash = 0u64;
        let css_set = CssSet::new();
        
        for subsys_id in 0..CGROUP_SUBSYS_COUNT as CgroupSubsysId {
            if let Some(css) = cgroup.css(subsys_id) {
                css_set.set_css(subsys_id, Some(css.clone()));
                hash ^= css.serial_nr();
            }
        }
        
        css_set.set_default_cgroup(Arc::downgrade(cgroup));
        
        // Check if css_set already exists
        if let Some(existing) = self.css_sets.read().get(&hash) {
            return Ok(existing.clone());
        }
        
        // Insert new css_set
        self.css_sets.write().insert(hash, css_set.clone());
        Ok(css_set)
    }
    
    /// Attach task to cgroup
    pub fn attach_task(&self, cgroup: &Arc<Cgroup>, pid: RawPid) -> Result<(), SystemError> {
        // Get or create css_set for this cgroup
        let css_set = self.find_or_create_css_set(cgroup)?;
        
        // Remove task from old css_set
        if let Some(old_css_set) = self.task_css_sets.read().get(&pid) {
            old_css_set.remove_task(pid);
        }
        
        // Add task to new css_set
        css_set.add_task(pid);
        self.task_css_sets.write().insert(pid, css_set.clone());
        
        // Notify subsystems
        for subsys_id in 0..CGROUP_SUBSYS_COUNT as CgroupSubsysId {
            if let Some(css) = cgroup.css(subsys_id) {
                if let Some(subsys) = self.get_subsystem(subsys_id) {
                    subsys.attach(&css, pid)?;
                }
            }
        }
        
        Ok(())
    }
    
    /// Detach task from cgroup
    pub fn detach_task(&self, pid: RawPid) -> Result<(), SystemError> {
        if let Some(css_set) = self.task_css_sets.write().remove(&pid) {
            css_set.remove_task(pid);
            
            // Notify subsystems
            for subsys_id in 0..CGROUP_SUBSYS_COUNT as CgroupSubsysId {
                if let Some(css) = css_set.css(subsys_id) {
                    if let Some(subsys) = self.get_subsystem(subsys_id) {
                        subsys.detach(&css, pid)?;
                    }
                }
            }
        }
        
        Ok(())
    }
    
    /// Handle process fork
    pub fn fork(&self, parent_pid: RawPid, child_pid: RawPid) -> Result<(), SystemError> {
        // Child inherits parent's css_set
        if let Some(css_set) = self.task_css_set(parent_pid) {
            css_set.add_task(child_pid);
            self.task_css_sets.write().insert(child_pid, css_set.clone());
            
            // Notify subsystems
            for subsys_id in 0..CGROUP_SUBSYS_COUNT as CgroupSubsysId {
                if let Some(css) = css_set.css(subsys_id) {
                    if let Some(subsys) = self.get_subsystem(subsys_id) {
                        subsys.fork(&css, child_pid)?;
                    }
                }
            }
        }
        
        Ok(())
    }
    
    /// Handle process exit
    pub fn exit(&self, pid: RawPid) -> Result<(), SystemError> {
        if let Some(css_set) = self.task_css_sets.write().remove(&pid) {
            css_set.remove_task(pid);
            
            // Notify subsystems
            for subsys_id in 0..CGROUP_SUBSYS_COUNT as CgroupSubsysId {
                if let Some(css) = css_set.css(subsys_id) {
                    if let Some(subsys) = self.get_subsystem(subsys_id) {
                        subsys.exit(&css, pid)?;
                    }
                }
            }
        }
        
        Ok(())
    }
}

/// Global cgroup manager instance
static CGROUP_MANAGER: SpinLock<Option<Arc<CgroupManager>>> = SpinLock::new(None);

/// Initialize core cgroup infrastructure (manager, subsystems)
pub fn cgroup_core_init() -> Result<(), SystemError> {
    log::info!("Creating CgroupManager...");
    let manager = CgroupManager::new()?;
    log::info!("CgroupManager created successfully");
    
    // Register built-in subsystems
    // Memory subsystem will be registered in mem_cgroup module
    
    log::info!("Setting global cgroup manager...");
    *CGROUP_MANAGER.lock() = Some(manager);
    log::info!("Global cgroup manager set successfully");
    
    Ok(())
}

/// Get global cgroup manager
pub fn cgroup_manager() -> Option<Arc<CgroupManager>> {
    CGROUP_MANAGER.lock().clone()
}

/// Attach current task to a cgroup
pub fn cgroup_attach_current(cgroup: &Arc<Cgroup>) -> Result<(), SystemError> {
    let manager = cgroup_manager().ok_or(SystemError::ENOSYS)?;
    let current_pid = crate::process::ProcessManager::current_pid();
    manager.attach_task(cgroup, current_pid)
}

/// Get current task's cgroup for a subsystem
pub fn current_task_cgroup(subsys_id: CgroupSubsysId) -> Option<Arc<Cgroup>> {
    let manager = cgroup_manager()?;
    let current_pid = crate::process::ProcessManager::current_pid();
    manager.task_cgroup(current_pid, subsys_id)
}

/// Cgroup filesystem interface
#[derive(Debug)]
pub struct CgroupFs {
    root: Arc<CgroupRoot>,
}

impl CgroupFs {
    pub fn new(root: Arc<CgroupRoot>) -> Self {
        Self { root }
    }
}

impl FileSystem for CgroupFs {
    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "cgroup2"
    }

    fn super_block(&self) -> crate::filesystem::vfs::SuperBlock {
        todo!("Implement cgroup filesystem super block")
    }

    fn root_inode(&self) -> Arc<dyn IndexNode> {
        todo!("Implement cgroup filesystem root inode")
    }

    fn info(&self) -> FsInfo {
        todo!("Implement cgroup filesystem info")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::sync::Arc;

    #[test]
    fn test_cgroup_creation() {
        let root = CgroupRoot::new("test".to_string(), 1).unwrap();
        let child = Cgroup::new("child".to_string(), Some(root.cgroup().clone()), Arc::downgrade(&root)).unwrap();
        
        assert_eq!(child.name(), "child");
        assert_eq!(child.level(), 1);
        assert!(child.parent().is_some());
        assert!(!child.is_root());
    }
    
    #[test]
    fn test_css_creation() {
        let root = CgroupRoot::new("test".to_string(), 1).unwrap();
        let css = CgroupSubsysState::new(None, Arc::downgrade(root.cgroup()), None, 0);
        
        assert_eq!(css.id(), 0);
        assert!(!css.is_online());
        
        css.set_online();
        assert!(css.is_online());
    }
    
    #[test]
    fn test_css_set() {
        let css_set = CssSet::new();
        let root = CgroupRoot::new("test".to_string(), 1).unwrap();
        let css = CgroupSubsysState::new(None, Arc::downgrade(root.cgroup()), None, 0);
        
        css_set.set_css(0, Some(css.clone()));
        assert!(css_set.css(0).is_some());
        
        let pid = RawPid::new(1);
        css_set.add_task(pid);
        assert_eq!(css_set.tasks().len(), 1);
    }
}