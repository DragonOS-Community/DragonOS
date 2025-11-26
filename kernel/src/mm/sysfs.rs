use alloc::{string::ToString, sync::Arc};
use unified_init::macros::unified_init;

use crate::{
    driver::base::firmware::sys_firmware_kobj,
    driver::base::{
        kobject::{KObjType, KObject, KObjectManager, KObjectSysFSOps},
        kset::KSet,
    },
    filesystem::{
        sysfs::{Attribute, AttributeGroup, SysFSOps, SysFSOpsSupport, SYSFS_ATTR_MODE_RO},
        vfs::syscall::InodeMode,
    },
    init::initcall::INITCALL_POSTCORE,
    libs::casting::DowncastArc,
};

use crate::driver::base::kobject::CommonKobj;
use crate::driver::base::kobject::KObjectState;
use crate::driver::base::kobject::LockedKObjectState;
use crate::filesystem::kernfs::KernFSInode;
use crate::init::boot::boot_callbacks;
use crate::libs::rwlock::RwLockReadGuard;
use crate::libs::rwlock::RwLockWriteGuard;
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
        let memmapd = kobj.downcast_arc::<MemmapDesc>().unwrap();
        let start = memmapd.inner().start;
        let start_string = format!("0x{:x}\n", start);
        let bytes = start_string.as_bytes();
        buf[..bytes.len()].copy_from_slice(bytes);
        Ok(bytes.len())
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
        let memmapd = kobj.downcast_arc::<MemmapDesc>().unwrap();
        let end = memmapd.inner().end;
        let end_string = format!("0x{:x}\n", end);
        let bytes = end_string.as_bytes();
        buf[..bytes.len()].copy_from_slice(bytes);
        Ok(bytes.len())
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
        let memmapd = kobj.downcast_arc::<MemmapDesc>().unwrap();
        let mt = memmapd.inner().memtype;
        match mt {
            1 => {
                let type_string = "System RAM\n".to_string();
                let bytes = type_string.as_bytes();
                buf[..bytes.len()].copy_from_slice(bytes);
                Ok(bytes.len())
            }
            2 => {
                let type_string = "Reserved\n".to_string();
                let bytes = type_string.as_bytes();
                buf[..bytes.len()].copy_from_slice(bytes);
                Ok(bytes.len())
            }
            3 => {
                let type_string = "ACPI Tables\n".to_string();
                let bytes = type_string.as_bytes();
                buf[..bytes.len()].copy_from_slice(bytes);
                Ok(bytes.len())
            }
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

    fn kobj_state(&self) -> RwLockReadGuard<'_, KObjectState> {
        self.kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwLockWriteGuard<'_, KObjectState> {
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
