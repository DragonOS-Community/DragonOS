use alloc::{string::ToString, sync::Arc};
use unified_init::macros::unified_init;

use crate::arch::MMArch;
use crate::{
    driver::base::{
        firmware::sys_firmware_kobj,
        kobject::{DynamicKObjKType, KObjType, KObject, KObjectManager, KObjectSysFSOps},
        kset::KSet,
    },
    filesystem::{
        sysfs::{
            file::sysfs_emit_str, Attribute, AttributeGroup, SysFSOps, SysFSOpsSupport,
            SYSFS_ATTR_MODE_RO,
        },
        vfs::InodeMode,
    },
    init::initcall::INITCALL_POSTCORE,
    libs::casting::DowncastArc,
    misc::ksysfs::sys_kernel_kobj,
    mm::{page_cache_stats, MemoryManagementArch},
};

use crate::driver::base::kobject::CommonKobj;
use crate::driver::base::kobject::KObjectState;
use crate::driver::base::kobject::LockedKObjectState;
use crate::filesystem::kernfs::KernFSInode;
use crate::init::boot::boot_callbacks;
use crate::libs::rwsem::{RwSemReadGuard, RwSemWriteGuard};
use crate::libs::spinlock::SpinLock;
use crate::libs::spinlock::SpinLockGuard;
use alloc::collections::btree_map;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Weak;
use core::any::Any;

use system_error::SystemError;

/// `/sys/firmware/memmap`的CommonKobj
static mut SYS_FIRMWARE_MEMMAP_KOBJ_INSTANCE: Option<Arc<CommonKobj>> = None;

#[inline(always)]
#[allow(dead_code)]
pub fn sys_firmware_memmap_kobj() -> Arc<CommonKobj> {
    unsafe { SYS_FIRMWARE_MEMMAP_KOBJ_INSTANCE.clone().unwrap() }
}

#[derive(Debug)]
pub struct MemmapDesc {
    inner: SpinLock<MemmapDescInner>,
    kobj_state: LockedKObjectState,
    name: String,
}

#[derive(Debug)]
pub struct MemmapDescInner {
    kern_inode: Option<Arc<KernFSInode>>,
    kset: Option<Arc<KSet>>,
    parent_kobj: Option<Weak<dyn KObject>>,
    // 私有属性
    pub start: usize,
    pub end: usize,
    pub memtype: usize,
}

impl MemmapDesc {
    pub fn new(name: String, s: usize, e: usize, t: usize) -> Arc<Self> {
        let md = MemmapDesc {
            inner: SpinLock::new(MemmapDescInner {
                kern_inode: None,
                kset: None,
                parent_kobj: None,
                start: s,
                end: e,
                memtype: t,
            }),
            kobj_state: LockedKObjectState::new(Some(KObjectState::INITIALIZED)),
            name: name.clone(),
        };
        Arc::new(md)
    }

    pub fn inner(&self) -> SpinLockGuard<'_, MemmapDescInner> {
        self.inner.lock_irqsave()
    }
}

#[derive(Debug)]
struct MemmapDescAttrGroup;

impl AttributeGroup for MemmapDescAttrGroup {
    fn name(&self) -> Option<&str> {
        None
    }

    fn attrs(&self) -> &[&'static dyn Attribute] {
        &[&AttrStart, &AttrEnd, &AttrType]
    }

    fn is_visible(
        &self,
        _kobj: Arc<dyn KObject>,
        attr: &'static dyn Attribute,
    ) -> Option<InodeMode> {
        Some(attr.mode())
    }
}

#[derive(Debug)]
pub struct MemmapDescKObjType;

impl KObjType for MemmapDescKObjType {
    fn sysfs_ops(&self) -> Option<&dyn SysFSOps> {
        Some(&KObjectSysFSOps)
    }

    fn attribute_groups(&self) -> Option<&'static [&'static dyn AttributeGroup]> {
        Some(&[&MemmapDescAttrGroup])
    }

    fn release(&self, _kobj: Arc<dyn KObject>) {}
}

#[derive(Debug)]
struct AttrStart;

impl Attribute for AttrStart {
    fn name(&self) -> &str {
        "start"
    }

    fn mode(&self) -> InodeMode {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let memmapd = kobj
            .downcast_arc::<MemmapDesc>()
            .ok_or(SystemError::EINVAL)?;
        let start = memmapd.inner().start;
        let start_string = format!("0x{:x}\n", start);
        sysfs_emit_str(buf, &start_string)
    }
}

#[derive(Debug)]
struct AttrEnd;

impl Attribute for AttrEnd {
    fn name(&self) -> &str {
        "end"
    }

    fn mode(&self) -> InodeMode {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let memmapd = kobj
            .downcast_arc::<MemmapDesc>()
            .ok_or(SystemError::EINVAL)?;
        let end = memmapd.inner().end;
        let end_string = format!("0x{:x}\n", end);
        sysfs_emit_str(buf, &end_string)
    }
}

#[derive(Debug)]
struct AttrType;

impl Attribute for AttrType {
    fn name(&self) -> &str {
        "type"
    }

    fn mode(&self) -> InodeMode {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let memmapd = kobj
            .downcast_arc::<MemmapDesc>()
            .ok_or(SystemError::EINVAL)?;
        let mt = memmapd.inner().memtype;
        match mt {
            1 => sysfs_emit_str(buf, "System RAM\n"),
            2 => sysfs_emit_str(buf, "Reserved\n"),
            3 => sysfs_emit_str(buf, "ACPI Tables\n"),
            _ => {
                log::error!("Unknown memmap type!");
                Err(SystemError::EINVAL)
            }
        }
    }
}

impl KObject for MemmapDesc {
    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner().kern_inode = inode;
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner().kern_inode.clone()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        self.inner().parent_kobj.clone()
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.inner().parent_kobj = parent;
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.inner().kset.clone()
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.inner().kset = kset;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        Some(&MemmapDescKObjType)
    }

    fn set_kobj_type(&self, _ktype: Option<&'static dyn KObjType>) {}

    fn name(&self) -> String {
        self.name.clone()
    }

    fn set_name(&self, _name: String) {}

    fn kobj_state(&self) -> RwSemReadGuard<'_, KObjectState> {
        self.kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwSemWriteGuard<'_, KObjectState> {
        self.kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.kobj_state_mut() = state;
    }
}

static mut MEMMAP_DESC_MANAGER: Option<MemmapDescManager> = None;

#[inline(always)]
pub fn memmap_desc_manager() -> &'static mut MemmapDescManager {
    return unsafe { MEMMAP_DESC_MANAGER.as_mut().unwrap() };
}

pub struct MemmapDescManager {
    memmap_descs: BTreeMap<usize, Arc<MemmapDesc>>,
}

impl MemmapDescManager {
    fn new() -> Self {
        MemmapDescManager {
            memmap_descs: BTreeMap::new(),
        }
    }

    pub fn insert(&mut self, index: usize, desc: Arc<MemmapDesc>) {
        self.memmap_descs.insert(index, desc);
    }

    pub fn iter_descs(&self) -> btree_map::Iter<'_, usize, Arc<MemmapDesc>> {
        self.memmap_descs.iter()
    }
}

#[inline(never)]
pub fn early_memmap_init() {
    let manager = MemmapDescManager::new();

    unsafe {
        MEMMAP_DESC_MANAGER = Some(manager);
    }
}

/// 初始化 memmap 模块在 sysfs 中的目录
#[unified_init(INITCALL_POSTCORE)]
fn memmap_sysfs_init() -> Result<(), SystemError> {
    // 下面这个函数应该换个地方, 因为做的的内容是一样的, 所以先放着
    boot_callbacks()
        .init_memmap_bp()
        .expect("init bp memmap failed");
    boot_callbacks()
        .init_memmap_sysfs()
        .expect("init sysfs memmap failed");

    let memmap_kobj = CommonKobj::new("memmap".to_string());

    let firm_kobj = sys_firmware_kobj();
    memmap_kobj.set_parent(Some(Arc::downgrade(&(firm_kobj as Arc<dyn KObject>))));
    KObjectManager::add_kobj(memmap_kobj.clone() as Arc<dyn KObject>).unwrap_or_else(|e| {
        log::warn!("Failed to add memmap kobject to sysfs: {:?}", e);
    });
    unsafe {
        SYS_FIRMWARE_MEMMAP_KOBJ_INSTANCE = Some(memmap_kobj);
    }

    // 把所有的memmap都注册到/sys/firmware/memmap下
    for (index, desc) in memmap_desc_manager().iter_descs() {
        memmap_sysfs_add(index, desc);
    }

    return Ok(());
}

fn memmap_sysfs_add(index: &usize, desc: &Arc<MemmapDesc>) {
    if unsafe { SYS_FIRMWARE_MEMMAP_KOBJ_INSTANCE.is_none() } {
        return;
    }

    let kobj = sys_firmware_memmap_kobj();
    desc.set_parent(Some(Arc::downgrade(&(kobj as Arc<dyn KObject>))));
    KObjectManager::add_kobj(desc.clone() as Arc<dyn KObject>).unwrap_or_else(|e| {
        log::warn!("Failed to add memmap({index:?}) kobject to sysfs: {:?}", e);
    });
}

#[derive(Debug)]
struct PagecacheAttrGroup;

impl AttributeGroup for PagecacheAttrGroup {
    fn name(&self) -> Option<&str> {
        None
    }

    fn attrs(&self) -> &[&'static dyn Attribute] {
        &[
            &AttrCachedKb,
            &AttrDirtyKb,
            &AttrWritebackKb,
            &AttrMappedKb,
            &AttrShmemKb,
        ]
    }

    fn is_visible(
        &self,
        _kobj: Arc<dyn KObject>,
        attr: &'static dyn Attribute,
    ) -> Option<InodeMode> {
        Some(attr.mode())
    }
}

#[derive(Debug)]
struct AttrCachedKb;

impl Attribute for AttrCachedKb {
    fn name(&self) -> &str {
        "cached_kb"
    }

    fn mode(&self) -> InodeMode {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, _kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let stats = page_cache_stats::snapshot();
        let page_kb = (MMArch::PAGE_SIZE >> 10) as u64;
        let cached = stats.file_pages.saturating_sub(stats.shmem_pages) * page_kb;
        sysfs_emit_str(buf, &format!("{cached}\n"))
    }
}

#[derive(Debug)]
struct AttrDirtyKb;

impl Attribute for AttrDirtyKb {
    fn name(&self) -> &str {
        "dirty_kb"
    }

    fn mode(&self) -> InodeMode {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, _kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let stats = page_cache_stats::snapshot();
        let page_kb = (MMArch::PAGE_SIZE >> 10) as u64;
        let dirty = stats.file_dirty * page_kb;
        sysfs_emit_str(buf, &format!("{dirty}\n"))
    }
}

#[derive(Debug)]
struct AttrWritebackKb;

impl Attribute for AttrWritebackKb {
    fn name(&self) -> &str {
        "writeback_kb"
    }

    fn mode(&self) -> InodeMode {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, _kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let stats = page_cache_stats::snapshot();
        let page_kb = (MMArch::PAGE_SIZE >> 10) as u64;
        let writeback = stats.file_writeback * page_kb;
        sysfs_emit_str(buf, &format!("{writeback}\n"))
    }
}

#[derive(Debug)]
struct AttrMappedKb;

impl Attribute for AttrMappedKb {
    fn name(&self) -> &str {
        "mapped_kb"
    }

    fn mode(&self) -> InodeMode {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, _kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let stats = page_cache_stats::snapshot();
        let page_kb = (MMArch::PAGE_SIZE >> 10) as u64;
        let mapped = stats.file_mapped * page_kb;
        sysfs_emit_str(buf, &format!("{mapped}\n"))
    }
}

#[derive(Debug)]
struct AttrShmemKb;

impl Attribute for AttrShmemKb {
    fn name(&self) -> &str {
        "shmem_kb"
    }

    fn mode(&self) -> InodeMode {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, _kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let stats = page_cache_stats::snapshot();
        let page_kb = (MMArch::PAGE_SIZE >> 10) as u64;
        let shmem = stats.shmem_pages * page_kb;
        sysfs_emit_str(buf, &format!("{shmem}\n"))
    }
}

#[unified_init(INITCALL_POSTCORE)]
fn pagecache_sysfs_init() -> Result<(), SystemError> {
    let kernel_kobj = sys_kernel_kobj();
    let mm_kobj = CommonKobj::new("mm".to_string());
    mm_kobj.set_parent(Some(Arc::downgrade(&(kernel_kobj as Arc<dyn KObject>))));
    KObjectManager::init_and_add_kobj(mm_kobj.clone(), Some(&DynamicKObjKType)).unwrap_or_else(
        |e| {
            log::warn!("Failed to add mm kobject to sysfs: {:?}", e);
        },
    );

    let pagecache_kobj = CommonKobj::new("pagecache".to_string());
    pagecache_kobj.set_parent(Some(Arc::downgrade(&(mm_kobj as Arc<dyn KObject>))));
    KObjectManager::init_and_add_kobj(pagecache_kobj.clone(), Some(&DynamicKObjKType))
        .unwrap_or_else(|e| {
            log::warn!("Failed to add pagecache kobject to sysfs: {:?}", e);
        });

    crate::filesystem::sysfs::sysfs_instance()
        .create_groups(
            &(pagecache_kobj as Arc<dyn KObject>),
            &[&PagecacheAttrGroup],
        )
        .map_err(|e| {
            log::warn!("Failed to create pagecache sysfs groups: {:?}", e);
            SystemError::ENOMEM
        })?;

    Ok(())
}
