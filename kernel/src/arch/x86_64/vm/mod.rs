use core::{
    arch::x86_64::{_xgetbv, _XCR_XFEATURE_ENABLED_MASK},
    sync::atomic::{AtomicU64, Ordering},
};

use alloc::{boxed::Box, sync::Arc};
use raw_cpuid::CpuId;
use system_error::SystemError;
use x86::msr::{rdmsr, IA32_CSTAR, IA32_PAT};

use crate::{
    kerror,
    libs::{lazy_init::Lazy, rwlock::RwLock},
};

use self::kvm_host::KvmFunc;

pub mod kvm_host;
pub mod vmx;

static KVM_X86_MANAGER: Lazy<KvmArchManager> = Lazy::new();

pub fn kvm_x86_ops() -> Option<&'static dyn KvmFunc> {
    *KVM_X86_MANAGER.funcs.read()
}

pub struct KvmArchManager {
    funcs: RwLock<Option<&'static dyn KvmFunc>>,
    host_xcr0: AtomicU64,
}

impl KvmArchManager {
    pub const KVM_MAX_VCPUS: usize = 1024;

    /// 厂商相关的init工作
    pub fn vendor_init(&self) -> Result<(), SystemError> {
        let cpuid = CpuId::new();
        let cpu_feature = cpuid.get_feature_info().ok_or(SystemError::ENOSYS)?;

        let kvm_x86_ops = kvm_x86_ops();

        // 是否已经设置过
        if let Some(ops) = kvm_x86_ops {
            kerror!("[KVM] already loaded vendor module {}", ops.name());
            return Err(SystemError::EEXIST);
        }

        // 确保cpu支持fpu浮点数处理器
        if !cpu_feature.has_fpu() || !cpu_feature.has_fxsave_fxstor() {
            kerror!("[KVM] inadequate fpu");
            return Err(SystemError::ENOSYS);
        }

        // TODO：实时内核需要判断tsc
        // https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/kvm/x86.c#9472

        // 读取主机page attribute table（页属性表）
        let host_pat = unsafe { rdmsr(IA32_PAT) };
        // PAT[0]是否为write back类型，即判断低三位是否为0b110(0x06)
        if host_pat & 0b111 != 0b110 {
            kerror!("[KVM] host PAT[0] is not WB");
            return Err(SystemError::EIO);
        }

        // TODO：mmu vendor init

        if cpu_feature.has_xsave() {
            self.host_xcr0.store(
                unsafe { _xgetbv(_XCR_XFEATURE_ENABLED_MASK) },
                Ordering::SeqCst,
            );
        }

        Ok(())
    }
}

/// ### Kvm的功能特性
#[derive(Debug)]
pub struct KvmCapabilities {
    has_tsc_control: bool,
    max_guest_tsc_khz: u32,
    tsc_scaling_ratio_frac_bits: u8,
    
}
