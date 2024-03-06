use alloc::{string::ToString, sync::Arc};
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::{
    driver::base::{
        kobject::{KObjType, KObject, KObjectManager, KObjectSysFSOps},
        kset::KSet,
    },
    filesystem::{
        sysfs::{
            file::sysfs_emit_str, Attribute, AttributeGroup, SysFSOps, SysFSOpsSupport,
            SYSFS_ATTR_MODE_RO,
        },
        vfs::syscall::ModeType,
    },
    init::initcall::INITCALL_POSTCORE,
    misc::ksysfs::sys_kernel_kset,
};

use super::{
    irqdesc::{irq_desc_manager, IrqDesc},
    IrqNumber,
};

/// `/sys/kernel/irq`的kset
static mut SYS_KERNEL_IRQ_KSET_INSTANCE: Option<Arc<KSet>> = None;

#[inline(always)]
#[allow(dead_code)]
pub fn sys_kernel_irq_kset() -> Arc<KSet> {
    unsafe { SYS_KERNEL_IRQ_KSET_INSTANCE.clone().unwrap() }
}

/// 中断描述符的kobjtype
///
/// https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/irq/irqdesc.c#280
#[derive(Debug)]
pub(super) struct IrqKObjType;

impl KObjType for IrqKObjType {
    fn sysfs_ops(&self) -> Option<&dyn SysFSOps> {
        Some(&KObjectSysFSOps)
    }

    fn attribute_groups(&self) -> Option<&'static [&'static dyn AttributeGroup]> {
        Some(&[&IrqAttrGroup])
    }

    fn release(&self, _kobj: Arc<dyn KObject>) {

        // https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/irq/irqdesc.c#428
    }
}

#[derive(Debug)]
struct IrqAttrGroup;

impl AttributeGroup for IrqAttrGroup {
    fn name(&self) -> Option<&str> {
        None
    }

    /// 所有的属性
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/irq/irqdesc.c#268
    fn attrs(&self) -> &[&'static dyn Attribute] {
        // 每个irq的属性
        // todo: 添加per_cpu_count属性
        &[
            &AttrChipName,
            &AttrHardwareIrq,
            &AttrType,
            &AttrWakeup,
            &AttrName,
            &AttrActions,
        ]
    }

    fn is_visible(
        &self,
        _kobj: Arc<dyn KObject>,
        attr: &'static dyn Attribute,
    ) -> Option<ModeType> {
        Some(attr.mode())
    }
}

/// 初始化irq模块在sysfs中的目录
///
/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/irq/irqdesc.c#313
#[unified_init(INITCALL_POSTCORE)]
fn irq_sysfs_init() -> Result<(), SystemError> {
    // todo!("irq_sysfs_init");

    let irq_kset = KSet::new("irq".to_string());
    irq_kset
        .register(Some(sys_kernel_kset()))
        .expect("register irq kset failed");
    unsafe {
        SYS_KERNEL_IRQ_KSET_INSTANCE = Some(irq_kset);
    }

    // 把所有的irq都注册到/sys/kernel/irq下
    for (irq, desc) in irq_desc_manager().iter_descs() {
        irq_sysfs_add(irq, desc);
    }

    return Ok(());
}

/// 把irqdesc添加到sysfs
fn irq_sysfs_add(irq: &IrqNumber, desc: &Arc<IrqDesc>) {
    if unsafe { SYS_KERNEL_IRQ_KSET_INSTANCE.is_none() } {
        return;
    }

    let kset = sys_kernel_irq_kset();
    KObjectManager::add_kobj(desc.clone() as Arc<dyn KObject>, Some(kset)).unwrap_or_else(|e| {
        kwarn!("Failed to add irq({irq:?}) kobject to sysfs: {:?}", e);
    });

    desc.mark_in_sysfs();
}

/// 从sysfs中删除irqdesc
#[allow(dead_code)]
pub(super) fn irq_sysfs_del(desc: &Arc<IrqDesc>) {
    if desc.in_sysfs() {
        KObjectManager::remove_kobj(desc.clone() as Arc<dyn KObject>);
        desc.mark_not_in_sysfs();
    }
}
#[derive(Debug)]
struct AttrChipName;

impl Attribute for AttrChipName {
    fn name(&self) -> &str {
        "chip_name"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let irq_desc = kobj
            .arc_any()
            .downcast::<IrqDesc>()
            .map_err(|_| SystemError::EINVAL)?;

        let chip = irq_desc.irq_data().chip_info_read_irqsave().chip();
        let name = chip.name();
        let len = core::cmp::min(name.len() + 1, buf.len());
        let name = format!("{}\n", name);
        buf[..len].copy_from_slice(name.as_bytes());
        return Ok(len);
    }
}

#[derive(Debug)]
struct AttrHardwareIrq;

impl Attribute for AttrHardwareIrq {
    fn name(&self) -> &str {
        "hwirq"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let irq_desc = kobj
            .arc_any()
            .downcast::<IrqDesc>()
            .map_err(|_| SystemError::EINVAL)?;
        let hwirq = irq_desc.hardware_irq();
        return sysfs_emit_str(buf, &format!("{}\n", hwirq.data()));
    }
}

#[derive(Debug)]
struct AttrType;

impl Attribute for AttrType {
    fn name(&self) -> &str {
        "type"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let irq_desc = kobj
            .arc_any()
            .downcast::<IrqDesc>()
            .map_err(|_| SystemError::EINVAL)?;
        let irq_type = if irq_desc.irq_data().is_level_type() {
            "level"
        } else {
            "edge"
        };

        return sysfs_emit_str(buf, &format!("{}\n", irq_type));
    }
}

#[derive(Debug)]
struct AttrWakeup;

impl Attribute for AttrWakeup {
    fn name(&self) -> &str {
        "wakeup"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let irq_desc = kobj
            .arc_any()
            .downcast::<IrqDesc>()
            .map_err(|_| SystemError::EINVAL)?;
        let wakeup = irq_desc.irq_data().is_wakeup_set();
        return sysfs_emit_str(buf, &format!("{}\n", wakeup));
    }
}

#[derive(Debug)]
struct AttrName;

impl Attribute for AttrName {
    fn name(&self) -> &str {
        "name"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let irq_desc = kobj
            .arc_any()
            .downcast::<IrqDesc>()
            .map_err(|_| SystemError::EINVAL)?;

        if let Some(name) = irq_desc.name() {
            return sysfs_emit_str(buf, &format!("{}\n", name));
        }

        return Ok(0);
    }
}

#[derive(Debug)]
struct AttrActions;

impl Attribute for AttrActions {
    fn name(&self) -> &str {
        "actions"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let irq_desc = kobj
            .arc_any()
            .downcast::<IrqDesc>()
            .map_err(|_| SystemError::EINVAL)?;

        let actions = irq_desc.actions();
        let mut len = 0;

        for action in actions {
            if len != 0 {
                len += sysfs_emit_str(&mut buf[len..], &format!(",{}", action.inner().name()))
                    .unwrap();
            } else {
                len +=
                    sysfs_emit_str(&mut buf[len..], &format!("{}", action.inner().name())).unwrap();
            }

            if len >= buf.len() {
                break;
            }
        }

        if len != 0 && len < buf.len() {
            len += sysfs_emit_str(&mut buf[len..], "\n").unwrap();
        }

        return Ok(len);
    }
}
