use alloc::{string::ToString, sync::Arc, sync::Weak};
use log::info;
use system_error::SystemError;

use crate::{
    driver::base::{
        device::{device_register, Device, DeviceType, IdTable},
        kobject::{
            CommonKobj, KObjType, KObject, KObjectCommonData, KObjectManager, KObjectState,
            LockedKObjectState,
        },
    },
    filesystem::{
        kernfs::KernFSInode,
        sysfs::{
            Attribute, AttributeGroup, SysFSOps, SysFSOpsSupport, SYSFS_ATTR_MODE_RW,
            SYSFS_ATTR_MODE_WO,
        },
        vfs::InodeMode,
    },
    init::initcall::INITCALL_FS,
    libs::{
        rwsem::{RwSemReadGuard, RwSemWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
    misc::ksysfs::sys_kernel_kobj,
};

static mut TEST_DEVICE: Option<Arc<TestDevice>> = None;
static mut TEST_CONTROL: Option<Arc<CommonKobj>> = None;

#[derive(Debug)]
struct TestDeviceInner {
    kobj_common: KObjectCommonData,
    registered: bool,
    fail_on_create: bool,
}

#[derive(Debug)]
#[cast_to([sync] Device)]

// 这是一个测试用的设备，仅为用户态暴露文件，以进行attribute_group的测试
// 会创建/sys/kernel/sysfs_group_test，通过写入文件动态在/sys/devices创建测试设备
struct TestDevice {
    inner: SpinLock<TestDeviceInner>,
    kobj_state: LockedKObjectState,
}

fn get_test_device() -> Arc<TestDevice> {
    unsafe { TEST_DEVICE.clone().unwrap() }
}

impl TestDevice {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: SpinLock::new(TestDeviceInner {
                kobj_common: KObjectCommonData::default(),
                registered: false,
                fail_on_create: false,
            }),
            kobj_state: LockedKObjectState::new(None),
        })
    }

    fn inner(&self) -> SpinLockGuard<'_, TestDeviceInner> {
        self.inner.lock()
    }

    fn register(self: &Arc<Self>) -> Result<(), SystemError> {
        let inner = self.inner();
        if inner.registered {
            return Err(SystemError::EEXIST);
        }
        drop(inner);

        let device = get_test_device();
        let result = device_register(device.clone());
        if let Err(e) = result {
            info!("Test device registration failed: {:?}", e);
            return Err(e);
        }

        device.set_kobj_type(Some(&TEST_KOBJ_TYPE));
        self.inner().registered = true;
        info!("Test device registered");
        Ok(())
    }

    fn unregister(self: &Arc<Self>) {
        if !self.inner().registered {
            return;
        }

        let device = get_test_device();

        let kobj = device.clone() as Arc<dyn KObject>;

        KObjectManager::remove_kobj(kobj);

        self.inner().registered = false;
        info!("Test device unregistered");
    }

    fn set_fail_on_create(&self, fail: bool) {
        self.inner().fail_on_create = fail;
        if fail {
            unsafe { TEST_ATTR_GROUPS = [&Group1, &Group3] }
        } else {
            unsafe { TEST_ATTR_GROUPS = [&Group1, &Group2] }
        }
    }

    fn fail_on_create(&self) -> bool {
        self.inner().fail_on_create
    }
}

impl KObject for TestDevice {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner().kobj_common.kern_inode = inode;
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner().kobj_common.kern_inode.clone()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        self.inner().kobj_common.parent.clone()
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.inner().kobj_common.parent = parent;
    }

    fn kset(&self) -> Option<Arc<crate::driver::base::kset::KSet>> {
        self.inner().kobj_common.kset.clone()
    }

    fn set_kset(&self, kset: Option<Arc<crate::driver::base::kset::KSet>>) {
        self.inner().kobj_common.kset = kset;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        self.inner().kobj_common.kobj_type
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner().kobj_common.kobj_type = ktype;
    }

    fn name(&self) -> alloc::string::String {
        "sysfs_group_test".to_string()
    }

    fn set_name(&self, _name: alloc::string::String) {}

    fn kobj_state(&self) -> RwSemReadGuard<'_, KObjectState> {
        self.kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwSemWriteGuard<'_, KObjectState> {
        self.kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.kobj_state.write() = state;
    }
}

impl Device for TestDevice {
    fn dev_type(&self) -> DeviceType {
        DeviceType::Other
    }

    fn id_table(&self) -> IdTable {
        IdTable::new("sysfs_group_test".to_string(), None)
    }

    fn set_bus(&self, _bus: Option<Weak<dyn crate::driver::base::device::bus::Bus>>) {}

    fn set_class(&self, _class: Option<Weak<dyn crate::driver::base::class::Class>>) {}

    fn driver(&self) -> Option<Arc<dyn crate::driver::base::device::driver::Driver>> {
        None
    }

    fn set_driver(&self, _driver: Option<Weak<dyn crate::driver::base::device::driver::Driver>>) {}

    fn is_dead(&self) -> bool {
        false
    }

    fn can_match(&self) -> bool {
        true
    }

    fn set_can_match(&self, _can_match: bool) {}

    fn state_synced(&self) -> bool {
        false
    }

    fn attribute_groups(&self) -> Option<&'static [&'static dyn AttributeGroup]> {
        unsafe { Some(&TEST_ATTR_GROUPS) }
    }

    fn dev_parent(&self) -> Option<Weak<dyn Device>> {
        None
    }

    fn set_dev_parent(&self, _parent: Option<Weak<dyn Device>>) {}
}

#[derive(Debug)]
struct TestKObjType;

impl KObjType for TestKObjType {
    fn release(&self, _kobj: Arc<dyn KObject>) {}
    fn sysfs_ops(&self) -> Option<&dyn SysFSOps> {
        Some(&TEST_SYSFS_OPS)
    }
    fn attribute_groups(&self) -> Option<&'static [&'static dyn AttributeGroup]> {
        unsafe { Some(&TEST_ATTR_GROUPS) }
    }
}

#[derive(Debug)]
struct ControlKObjType;

impl KObjType for ControlKObjType {
    fn release(&self, _kobj: Arc<dyn KObject>) {}
    fn sysfs_ops(&self) -> Option<&dyn SysFSOps> {
        Some(&TEST_SYSFS_OPS)
    }
    fn attribute_groups(&self) -> Option<&'static [&'static dyn AttributeGroup]> {
        Some(&CONTROL_ATTR_GROUPS)
    }
}

static TEST_KOBJ_TYPE: TestKObjType = TestKObjType;
static CONTROL_KOBJ_TYPE: ControlKObjType = ControlKObjType;

#[derive(Debug)]
struct TestSysFSOps;

impl SysFSOps for TestSysFSOps {
    fn show(
        &self,
        kobj: Arc<dyn KObject>,
        attr: &dyn Attribute,
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        attr.show(kobj, buf)
    }

    fn store(
        &self,
        kobj: Arc<dyn KObject>,
        attr: &dyn Attribute,
        buf: &[u8],
    ) -> Result<usize, SystemError> {
        attr.store(kobj, buf)
    }
}

static TEST_SYSFS_OPS: TestSysFSOps = TestSysFSOps;

static CONTROL_ATTR_GROUPS: [&'static dyn AttributeGroup; 1] = [&ControlGroup];
static mut TEST_ATTR_GROUPS: [&'static dyn AttributeGroup; 2] = [&Group1, &Group2];
#[derive(Debug)]
struct Group1;

impl AttributeGroup for Group1 {
    fn name(&self) -> Option<&str> {
        Some("test_group1")
    }

    fn attrs(&self) -> &[&'static dyn Attribute] {
        &ATTRS
    }
}

#[derive(Debug)]
struct Group2;

impl AttributeGroup for Group2 {
    fn name(&self) -> Option<&str> {
        Some("test_group2")
    }

    fn attrs(&self) -> &[&'static dyn Attribute] {
        &ATTRS
    }
}

#[derive(Debug)]
struct Group3;

// 创建同名group，触发回滚
impl AttributeGroup for Group3 {
    fn name(&self) -> Option<&str> {
        Some("test_group1")
    }

    fn attrs(&self) -> &[&'static dyn Attribute] {
        &ATTRS
    }
}

static ATTRS: [&'static dyn Attribute; 1] = [&AttrStatus];

#[derive(Debug)]
struct AttrStatus;

impl Attribute for AttrStatus {
    fn name(&self) -> &str {
        "status"
    }

    fn mode(&self) -> InodeMode {
        SYSFS_ATTR_MODE_RW
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW | SysFSOpsSupport::ATTR_STORE
    }

    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        Ok(0)
    }

    fn store(&self, _kobj: Arc<dyn KObject>, _buf: &[u8]) -> Result<usize, SystemError> {
        Ok(0)
    }
}

#[derive(Debug)]
struct ControlGroup;

impl AttributeGroup for ControlGroup {
    fn name(&self) -> Option<&str> {
        None
    }

    fn attrs(&self) -> &[&'static dyn Attribute] {
        &CONTROL_ATTRS
    }
}

static CONTROL_ATTRS: [&'static dyn Attribute; 3] =
    [&AttrRegister, &AttrUnregister, &AttrFailOnCreate];

#[derive(Debug)]
struct AttrRegister;

#[derive(Debug)]
struct AttrUnregister;

#[derive(Debug)]
struct AttrFailOnCreate;

impl Attribute for AttrRegister {
    fn name(&self) -> &str {
        "register"
    }

    fn mode(&self) -> InodeMode {
        SYSFS_ATTR_MODE_WO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_STORE
    }

    fn store(&self, _kobj: Arc<dyn KObject>, buf: &[u8]) -> Result<usize, SystemError> {
        let device = get_test_device();
        device.register()?;
        Ok(buf.len())
    }
}

impl Attribute for AttrUnregister {
    fn name(&self) -> &str {
        "unregister"
    }

    fn mode(&self) -> InodeMode {
        SYSFS_ATTR_MODE_WO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_STORE
    }

    fn store(&self, _kobj: Arc<dyn KObject>, buf: &[u8]) -> Result<usize, SystemError> {
        let device = get_test_device();
        device.unregister();
        Ok(buf.len())
    }
}

impl Attribute for AttrFailOnCreate {
    fn name(&self) -> &str {
        "fail_on_create"
    }

    fn mode(&self) -> InodeMode {
        SYSFS_ATTR_MODE_RW
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW | SysFSOpsSupport::ATTR_STORE
    }

    fn show(&self, _kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let device = get_test_device();
        let fail = device.fail_on_create();
        let s = alloc::format!("{}\n", if fail { 1 } else { 0 });
        let bytes = s.as_bytes();
        let len = bytes.len().min(buf.len());
        buf[..len].copy_from_slice(&bytes[..len]);
        Ok(len)
    }

    fn store(&self, _kobj: Arc<dyn KObject>, buf: &[u8]) -> Result<usize, SystemError> {
        let device = get_test_device();
        let s = core::str::from_utf8(buf).map_err(|_| SystemError::EINVAL)?;
        let s = s.trim();
        let fail = match s {
            "1" | "true" => true,
            "0" | "false" => false,
            _ => return Err(SystemError::EINVAL),
        };
        device.set_fail_on_create(fail);
        Ok(buf.len())
    }
}

#[unified_init::macros::unified_init(INITCALL_FS)]
fn test_init() -> Result<(), SystemError> {
    info!("Initializing sysfs group test device");

    let device = TestDevice::new();

    unsafe {
        TEST_DEVICE = Some(device.clone());
    }

    let control = crate::driver::base::kobject::CommonKobj::new("sysfs_group_test".to_string());
    control.set_kobj_type(Some(&CONTROL_KOBJ_TYPE));

    let kernel_kobj = sys_kernel_kobj();
    control.set_parent(Some(Arc::downgrade(&(kernel_kobj as Arc<dyn KObject>))));

    crate::driver::base::kobject::KObjectManager::init_and_add_kobj(
        control.clone() as Arc<dyn KObject>,
        Some(&CONTROL_KOBJ_TYPE),
    )?;

    unsafe {
        TEST_CONTROL = Some(control);
    }

    info!("Sysfs group test device initialized");
    Ok(())
}
