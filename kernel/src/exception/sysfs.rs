use alloc::sync::Arc;
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::{
    driver::base::kobject::{KObjType, KObject, KObjectSysFSOps},
    filesystem::{
        sysfs::{Attribute, AttributeGroup, SysFSOps},
        vfs::syscall::ModeType,
    },
    init::initcall::INITCALL_POSTCORE,
};

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

    fn attrs(&self) -> &[&'static dyn Attribute] {
        todo!("irq_attr_group.attrs")
        // todo: https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/irq/irqdesc.c#268
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
    kwarn!("Unimplemented: irq_sysfs_init");
    Ok(())
}
