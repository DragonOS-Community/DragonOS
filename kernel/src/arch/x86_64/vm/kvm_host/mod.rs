use system_error::SystemError;

pub mod vcpu;

type SysResult<T> = Result<T, SystemError>;

pub struct X86KvmArch {
    /// 中断芯片模式
    irqchip_mode: KvmIrqChipMode,
    /// 负责引导(bootstrap)kvm的vcpu——id
    bsp_vcpu_id: usize,
}

#[derive(Debug, Clone, Copy)]
pub enum KvmIrqChipMode {
    None,
    Kernel,
    Split,
}

pub trait KvmFunc: Send + Sync {
    /// 返回该硬件支持的名字，例如“Vmx”
    fn name(&self) -> &'static str;

    /// 启用硬件支持
    /// （注：只有dummy实现能够返回ENOSYS错误码，表示未指定）
    fn hardware_enable(&self) -> SysResult<()>;
}

pub struct DummyKvmFunc;

impl KvmFunc for DummyKvmFunc {
    fn name(&self) -> &'static str {
        "kvm_dummy_ops"
    }

    fn hardware_enable(&self) -> SysResult<()> {
        Err(SystemError::ENOSYS)
    }
}
