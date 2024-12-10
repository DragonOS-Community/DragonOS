use core::arch::asm;

use alloc::slice;
use log::{debug, error};
use raw_cpuid::CpuId;
use system_error::SystemError;
use x86::{
    bits64::vmx::vmxon,
    controlregs::{cr0, cr0_write, cr4, cr4_write, Cr0, Cr4},
    msr::{
        rdmsr, wrmsr, IA32_FEATURE_CONTROL, IA32_VMX_CR0_FIXED0, IA32_VMX_CR0_FIXED1,
        IA32_VMX_CR4_FIXED0, IA32_VMX_CR4_FIXED1,
    },
    vmx::vmcs::ro,
};

use crate::{
    arch::{mm::barrier, MMArch},
    mm::{MemoryManagementArch, PhysAddr},
};

use super::vmx::vmx_info;

pub struct KvmX86Asm;

impl KvmX86Asm {
    pub fn read_pkru() -> u32 {
        let cpuid = CpuId::new();
        if let Some(feat) = cpuid.get_extended_feature_info() {
            if feat.has_ospke() {
                return Self::rdpkru();
            }
        }
        return 0;
    }

    pub fn write_pkru(_val: u32) {
        let cpuid = CpuId::new();
        if let Some(feat) = cpuid.get_extended_feature_info() {
            if feat.has_ospke() {
                todo!();
            }
        }
    }

    fn rdpkru() -> u32 {
        let ecx: u32 = 0;
        let pkru: u32;
        let _edx: u32;

        unsafe {
            asm!(
                "rdpkru",
                out("eax") pkru,
                out("edx") _edx,
                in("ecx") ecx,
            );
        }

        pkru
    }

    pub fn get_segment_base(gdt_base: *const u64, gdt_size: u16, segment_selector: u16) -> u64 {
        let table = segment_selector & 0x0004; // get table indicator in selector
        let index = (segment_selector >> 3) as usize; // get index in selector
        if table == 0 && index == 0 {
            return 0;
        }
        let descriptor_table = unsafe { slice::from_raw_parts(gdt_base, gdt_size.into()) };
        let descriptor = descriptor_table[index];

        let base_high = (descriptor & 0xFF00_0000_0000_0000) >> 32;
        let base_mid = (descriptor & 0x0000_00FF_0000_0000) >> 16;
        let base_low = (descriptor & 0x0000_0000_FFFF_0000) >> 16;
        let segment_base = (base_high | base_mid | base_low) & 0xFFFFFFFF;
        let virtaddr = unsafe {
            MMArch::phys_2_virt(PhysAddr::new(segment_base as usize))
                .unwrap()
                .data() as u64
        };
        return virtaddr;
    }
}

pub struct VmxAsm;

impl VmxAsm {
    pub fn vmclear(phys_addr: PhysAddr) {
        debug!("vmclear addr {phys_addr:?}");
        match unsafe { x86::bits64::vmx::vmclear(phys_addr.data() as u64) } {
            Ok(_) => {}
            Err(e) => {
                panic!("[VMX] vmclear failed! reason: {e:?}");
            }
        }
    }

    pub fn vmcs_load(phys_addr: PhysAddr) {
        match unsafe { x86::bits64::vmx::vmptrld(phys_addr.data() as u64) } {
            Ok(_) => {}
            Err(e) => {
                panic!("[VMX] vmptrld failed! reason: {e:?}");
            }
        }
    }

    /// vmrite the current VMCS.
    pub fn vmx_vmwrite(vmcs_field: u32, value: u64) {
        unsafe {
            x86::bits64::vmx::vmwrite(vmcs_field, value)
                .unwrap_or_else(|_| panic!("vmcs_field: {:x} vmx_write fail", vmcs_field))
        }
    }

    /// vmread the current VMCS.
    pub fn vmx_vmread(vmcs_field: u32) -> u64 {
        unsafe { x86::bits64::vmx::vmread(vmcs_field).expect("vmx_read fail: ") }
    }

    pub fn kvm_cpu_vmxon(phys_addr: PhysAddr) -> Result<(), SystemError> {
        unsafe {
            let mut cr4 = cr4();
            cr4.insert(Cr4::CR4_ENABLE_VMX);
            cr4_write(cr4);

            Self::vmx_set_lock_bit()?;
            Self::vmx_set_cr0_bits();
            Self::vmx_set_cr4_bits();
            debug!("vmxon addr {phys_addr:?}");

            vmxon(phys_addr.data() as u64).expect("[VMX] vmxon failed! reason");

            barrier::mfence();

            Ok(())
        }
    }

    #[allow(dead_code)]
    const VMX_VPID_EXTENT_INDIVIDUAL_ADDR: u64 = 0;
    const VMX_VPID_EXTENT_SINGLE_CONTEXT: u64 = 1;
    #[allow(dead_code)]
    const VMX_VPID_EXTENT_ALL_CONTEXT: u64 = 2;
    #[allow(dead_code)]
    const VMX_VPID_EXTENT_SINGLE_NON_GLOBAL: u64 = 3;
    #[allow(dead_code)]
    const VMX_EPT_EXTENT_CONTEXT: u64 = 1;
    const VMX_EPT_EXTENT_GLOBAL: u64 = 2;
    #[allow(dead_code)]
    const VMX_EPT_EXTENT_SHIFT: u64 = 24;

    pub fn ept_sync_global() {
        Self::invept(Self::VMX_EPT_EXTENT_GLOBAL, 0, 0);
    }
    #[allow(dead_code)]
    pub fn ept_sync_context(eptp: u64) {
        if vmx_info().has_vmx_invept_context() {
            Self::invept(Self::VMX_EPT_EXTENT_CONTEXT, eptp, 0);
        } else {
            Self::ept_sync_global();
        }
    }

    pub fn sync_vcpu_single(vpid: u16) {
        if vpid == 0 {
            return;
        }

        Self::invvpid(Self::VMX_VPID_EXTENT_SINGLE_CONTEXT, vpid, 0)
    }

    pub fn sync_vcpu_global() {
        Self::invvpid(Self::VMX_VPID_EXTENT_ALL_CONTEXT, 0, 0);
    }

    #[inline(always)]
    fn invept(ext: u64, eptp: u64, gpa: u64) {
        #[repr(C)]
        struct InveptDescriptor {
            eptp: u64,
            gpa: u64,
        }

        let descriptor = InveptDescriptor { eptp, gpa };

        unsafe {
            asm!(
                "invept {0}, [{1}]",
                in(reg) ext,
                in(reg) &descriptor,
                options(nostack)
            );
        }
    }

    #[inline(always)]
    fn invvpid(ext: u64, vpid: u16, gva: u64) {
        #[repr(C)]
        struct InvvpidDescriptor {
            vpid: u16,
            rsvd: u64,
            gva: u64,
        }

        let descriptor = InvvpidDescriptor { vpid, rsvd: 0, gva };

        unsafe {
            asm!(
                "invvpid {0}, [{1}]",
                in(reg) ext,
                in(reg) &descriptor,
                options(nostack)
            );
        }
    }

    /// Set the mandatory bits in CR4 and clear bits that are mandatory zero
    /// (Intel Manual: 24.8 Restrictions on VMX Operation)
    fn vmx_set_cr4_bits() {
        let ia32_vmx_cr4_fixed0 = unsafe { rdmsr(IA32_VMX_CR4_FIXED0) };
        let ia32_vmx_cr4_fixed1 = unsafe { rdmsr(IA32_VMX_CR4_FIXED1) };

        let mut cr4 = unsafe { cr4() };

        cr4 |= Cr4::from_bits_truncate(ia32_vmx_cr4_fixed0 as usize);
        cr4 &= Cr4::from_bits_truncate(ia32_vmx_cr4_fixed1 as usize);

        unsafe { cr4_write(cr4) };
    }

    /// Check if we need to set bits in IA32_FEATURE_CONTROL
    // (Intel Manual: 24.7 Enabling and Entering VMX Operation)
    fn vmx_set_lock_bit() -> Result<(), SystemError> {
        const VMX_LOCK_BIT: u64 = 1 << 0;
        const VMXON_OUTSIDE_SMX: u64 = 1 << 2;

        let ia32_feature_control = unsafe { rdmsr(IA32_FEATURE_CONTROL) };

        if (ia32_feature_control & VMX_LOCK_BIT) == 0 {
            unsafe {
                wrmsr(
                    IA32_FEATURE_CONTROL,
                    VMXON_OUTSIDE_SMX | VMX_LOCK_BIT | ia32_feature_control,
                )
            };
        } else if (ia32_feature_control & VMXON_OUTSIDE_SMX) == 0 {
            return Err(SystemError::EPERM);
        }

        Ok(())
    }

    /// Set the mandatory bits in CR0 and clear bits that are mandatory zero
    /// (Intel Manual: 24.8 Restrictions on VMX Operation)
    fn vmx_set_cr0_bits() {
        let ia32_vmx_cr0_fixed0 = unsafe { rdmsr(IA32_VMX_CR0_FIXED0) };
        let ia32_vmx_cr0_fixed1 = unsafe { rdmsr(IA32_VMX_CR0_FIXED1) };

        let mut cr0 = unsafe { cr0() };

        cr0 |= Cr0::from_bits_truncate(ia32_vmx_cr0_fixed0 as usize);
        cr0 &= Cr0::from_bits_truncate(ia32_vmx_cr0_fixed1 as usize);

        unsafe { cr0_write(cr0) };
    }
}

#[no_mangle]
unsafe extern "C" fn vmx_vmlaunch() {
    if let Err(e) = x86::bits64::vmx::vmlaunch() {
        error!(
            "vmx_launch fail: {:?}, err code {}",
            e,
            VmxAsm::vmx_vmread(ro::VM_INSTRUCTION_ERROR)
        );
    }
}

bitflags! {
    pub struct IntrInfo: u32 {
        const INTR_INFO_VECTOR_MASK = 0xff;
        const INTR_INFO_INTR_TYPE_MASK = 0x700;
        const INTR_INFO_DELIVER_CODE_MASK = 0x800;
        const INTR_INFO_UNBLOCK_NMI = 0x1000;
        const INTR_INFO_VALID_MASK = 0x80000000;
        const INTR_INFO_RESVD_BITS_MASK = 0x7ffff000;
    }

    pub struct IntrType: u32 {
        /// external interrupt
        const INTR_TYPE_EXT_INTR = (0 << 8);
        /// reserved
        const INTR_TYPE_RESERVED = (1 << 8);
        /// NMI
        const INTR_TYPE_NMI_INTR = (2 << 8);
        /// processor exception
        const INTR_TYPE_HARD_EXCEPTION = (3 << 8);
        /// software interrupt
        const INTR_TYPE_SOFT_INTR = (4 << 8);
        /// ICE breakpoint - undocumented
        const INTR_TYPE_PRIV_SW_EXCEPTION = (5 << 8);
        /// software exception
        const INTR_TYPE_SOFT_EXCEPTION = (6 << 8);
        /// other even
        const INTR_TYPE_OTHER_EVENT = (7 << 8);
    }

    pub struct MiscEnable: u64 {
        const MSR_IA32_MISC_ENABLE_FAST_STRING = 1 << 0;
        const MSR_IA32_MISC_ENABLE_TCC = 1 << 1;
        const MSR_IA32_MISC_ENABLE_EMON = 1 << 7;
        const MSR_IA32_MISC_ENABLE_BTS_UNAVAIL = 1 << 11;
        const MSR_IA32_MISC_ENABLE_PEBS_UNAVAIL = 1 << 12;
        const MSR_IA32_MISC_ENABLE_ENHANCED_SPEEDSTEP = 1 << 16;
        const MSR_IA32_MISC_ENABLE_MWAIT = 1 << 18;
        const MSR_IA32_MISC_ENABLE_LIMIT_CPUID= 1 << 22;
        const MSR_IA32_MISC_ENABLE_XTPR_DISABLE = 1 << 23;
        const MSR_IA32_MISC_ENABLE_XD_DISABLE = 1 << 34;
    }

    pub struct ArchCapabilities: u64 {
        /// Not susceptible to Meltdown
        const ARCH_CAP_RDCL_NO = 1 << 0;
        /// Enhanced IBRS support
        const ARCH_CAP_IBRS_ALL = 1 << 1;
        /// RET may use alternative branch predictors
        const ARCH_CAP_RSBA	= 1 << 2;
        /// Skip L1D flush on vmentry
        const ARCH_CAP_SKIP_VMENTRY_L1DFLUSH = 1 << 3;
        ///
        /// Not susceptible to Speculative Store Bypass
        /// attack, so no Speculative Store Bypass
        /// control required.
        ///
        const ARCH_CAP_SSB_NO = 1 << 4;
        /// Not susceptible to
        /// Microarchitectural Data
        /// Sampling (MDS) vulnerabilities.
        const ARCH_CAP_MDS_NO = 1 << 5;
        /// The processor is not susceptible to a
        /// machine check error due to modifying the
        /// code page size along with either the
        /// physical address or cache type
        /// without TLB invalidation.
        const ARCH_CAP_PSCHANGE_MC_NO = 1 << 6;
        /// MSR for TSX control is available.
        const ARCH_CAP_TSX_CTRL_MSR = 1 << 7;
        /// Not susceptible to
        /// TSX Async Abort (TAA) vulnerabilities.
        const ARCH_CAP_TAA_NO = 1 << 8;
        /// Not susceptible to SBDR and SSDP
        /// variants of Processor MMIO stale data
        /// vulnerabilities.
        const ARCH_CAP_SBDR_SSDP_NO = 1 << 13;
        /// Not susceptible to FBSDP variant of
        /// Processor MMIO stale data
        /// vulnerabilities.
        const ARCH_CAP_FBSDP_NO = 1 << 14;
        /// Not susceptible to PSDP variant of
        /// Processor MMIO stale data
        /// vulnerabilities.
        const ARCH_CAP_PSDP_NO = 1 << 15;
        /// VERW clears CPU fill buffer
        /// even on MDS_NO CPUs.
        const ARCH_CAP_FB_CLEAR = 1 << 17;
        /// MSR_IA32_MCU_OPT_CTRL[FB_CLEAR_DIS]
        /// bit available to control VERW
        /// behavior.
        const ARCH_CAP_FB_CLEAR_CTRL = 1 << 18;
        /// Indicates RET may use predictors
        /// other than the RSB. With eIBRS
        /// enabled predictions in kernel mode
        /// are restricted to targets in
        /// kernel.
        const ARCH_CAP_RRSBA = 1 << 19;
        /// Not susceptible to Post-Barrier
        /// Return Stack Buffer Predictions.
        const ARCH_CAP_PBRSB_NO = 1 << 24;
        /// CPU is vulnerable to Gather
        /// Data Sampling (GDS) and
        /// has controls for mitigation.
        const ARCH_CAP_GDS_CTRL = 1 << 25;
        /// CPU is not vulnerable to Gather
        /// Data Sampling (GDS).
        const ARCH_CAP_GDS_NO = 1 << 26;
        /// IA32_XAPIC_DISABLE_STATUS MSR
        /// supported
        const ARCH_CAP_XAPIC_DISABLE = 1 << 21;

        const KVM_SUPPORTED_ARCH_CAP = ArchCapabilities::ARCH_CAP_RDCL_NO.bits
        | ArchCapabilities::ARCH_CAP_IBRS_ALL.bits
        | ArchCapabilities::ARCH_CAP_RSBA.bits
        | ArchCapabilities::ARCH_CAP_SKIP_VMENTRY_L1DFLUSH.bits
        | ArchCapabilities::ARCH_CAP_SSB_NO.bits
        | ArchCapabilities::ARCH_CAP_MDS_NO.bits
        | ArchCapabilities::ARCH_CAP_PSCHANGE_MC_NO.bits
        | ArchCapabilities::ARCH_CAP_TSX_CTRL_MSR.bits
        | ArchCapabilities::ARCH_CAP_TAA_NO.bits
        | ArchCapabilities::ARCH_CAP_SBDR_SSDP_NO.bits
        | ArchCapabilities::ARCH_CAP_FBSDP_NO.bits
        | ArchCapabilities::ARCH_CAP_PSDP_NO.bits
        | ArchCapabilities::ARCH_CAP_FB_CLEAR.bits
        | ArchCapabilities::ARCH_CAP_RRSBA.bits
        | ArchCapabilities::ARCH_CAP_PBRSB_NO.bits
        | ArchCapabilities::ARCH_CAP_GDS_NO.bits;
    }
}

#[derive(Debug, Default, Clone)]
pub struct MsrData {
    pub host_initiated: bool,
    pub index: u32,
    pub data: u64,
}

#[repr(C, align(16))]
#[derive(Debug, Default, Copy, Clone)]
pub struct VmxMsrEntry {
    pub index: u32,
    pub reserved: u32,
    pub data: u64,
}

#[allow(dead_code)]
pub mod hyperv {
    /* Hyper-V specific model specific registers (MSRs) */

    /* MSR used to identify the guest OS. */
    pub const HV_X64_MSR_GUEST_OS_ID: u32 = 0x40000000;

    /* MSR used to setup pages used to communicate with the hypervisor. */
    pub const HV_X64_MSR_HYPERCALL: u32 = 0x40000001;

    /* MSR used to provide vcpu index */
    pub const HV_REGISTER_VP_INDEX: u32 = 0x40000002;

    /* MSR used to reset the guest OS. */
    pub const HV_X64_MSR_RESET: u32 = 0x40000003;

    /* MSR used to provide vcpu runtime in 100ns units */
    pub const HV_X64_MSR_VP_RUNTIME: u32 = 0x40000010;

    /* MSR used to read the per-partition time reference counter */
    pub const HV_REGISTER_TIME_REF_COUNT: u32 = 0x40000020;

    /* A partition's reference time stamp counter (TSC) page */
    pub const HV_REGISTER_REFERENCE_TSC: u32 = 0x40000021;

    /* MSR used to retrieve the TSC frequency */
    pub const HV_X64_MSR_TSC_FREQUENCY: u32 = 0x40000022;

    /* MSR used to retrieve the local APIC timer frequency */
    pub const HV_X64_MSR_APIC_FREQUENCY: u32 = 0x40000023;

    /* Define the virtual APIC registers */
    pub const HV_X64_MSR_EOI: u32 = 0x40000070;
    pub const HV_X64_MSR_ICR: u32 = 0x40000071;
    pub const HV_X64_MSR_TPR: u32 = 0x40000072;
    pub const HV_X64_MSR_VP_ASSIST_PAGE: u32 = 0x40000073;

    /* Define synthetic interrupt controller model specific registers. */
    pub const HV_REGISTER_SCONTROL: u32 = 0x40000080;
    pub const HV_REGISTER_SVERSION: u32 = 0x40000081;
    pub const HV_REGISTER_SIEFP: u32 = 0x40000082;
    pub const HV_REGISTER_SIMP: u32 = 0x40000083;
    pub const HV_REGISTER_EOM: u32 = 0x40000084;
    pub const HV_REGISTER_SINT0: u32 = 0x40000090;
    pub const HV_REGISTER_SINT1: u32 = 0x40000091;
    pub const HV_REGISTER_SINT2: u32 = 0x40000092;
    pub const HV_REGISTER_SINT3: u32 = 0x40000093;
    pub const HV_REGISTER_SINT4: u32 = 0x40000094;
    pub const HV_REGISTER_SINT5: u32 = 0x40000095;
    pub const HV_REGISTER_SINT6: u32 = 0x40000096;
    pub const HV_REGISTER_SINT7: u32 = 0x40000097;
    pub const HV_REGISTER_SINT8: u32 = 0x40000098;
    pub const HV_REGISTER_SINT9: u32 = 0x40000099;
    pub const HV_REGISTER_SINT10: u32 = 0x4000009A;
    pub const HV_REGISTER_SINT11: u32 = 0x4000009B;
    pub const HV_REGISTER_SINT12: u32 = 0x4000009C;
    pub const HV_REGISTER_SINT13: u32 = 0x4000009D;
    pub const HV_REGISTER_SINT14: u32 = 0x4000009E;
    pub const HV_REGISTER_SINT15: u32 = 0x4000009F;

    /*
     * Define synthetic interrupt controller model specific registers for
     * nested hypervisor.
     */
    pub const HV_REGISTER_NESTED_SCONTROL: u32 = 0x40001080;
    pub const HV_REGISTER_NESTED_SVERSION: u32 = 0x40001081;
    pub const HV_REGISTER_NESTED_SIEFP: u32 = 0x40001082;
    pub const HV_REGISTER_NESTED_SIMP: u32 = 0x40001083;
    pub const HV_REGISTER_NESTED_EOM: u32 = 0x40001084;
    pub const HV_REGISTER_NESTED_SINT0: u32 = 0x40001090;

    /*
     * Synthetic Timer MSRs. Four timers per vcpu.
     */
    pub const HV_REGISTER_STIMER0_CONFIG: u32 = 0x400000B0;
    pub const HV_REGISTER_STIMER0_COUNT: u32 = 0x400000B1;
    pub const HV_REGISTER_STIMER1_CONFIG: u32 = 0x400000B2;
    pub const HV_REGISTER_STIMER1_COUNT: u32 = 0x400000B3;
    pub const HV_REGISTER_STIMER2_CONFIG: u32 = 0x400000B4;
    pub const HV_REGISTER_STIMER2_COUNT: u32 = 0x400000B5;
    pub const HV_REGISTER_STIMER3_CONFIG: u32 = 0x400000B6;
    pub const HV_REGISTER_STIMER3_COUNT: u32 = 0x400000B7;

    /* Hyper-V guest idle MSR */
    pub const HV_X64_MSR_GUEST_IDLE: u32 = 0x400000F0;

    /* Hyper-V guest crash notification MSR's */
    pub const HV_REGISTER_CRASH_P0: u32 = 0x40000100;
    pub const HV_REGISTER_CRASH_P1: u32 = 0x40000101;
    pub const HV_REGISTER_CRASH_P2: u32 = 0x40000102;
    pub const HV_REGISTER_CRASH_P3: u32 = 0x40000103;
    pub const HV_REGISTER_CRASH_P4: u32 = 0x40000104;
    pub const HV_REGISTER_CRASH_CTL: u32 = 0x40000105;

    /* TSC emulation after migration */
    pub const HV_X64_MSR_REENLIGHTENMENT_CONTROL: u32 = 0x40000106;
    pub const HV_X64_MSR_TSC_EMULATION_CONTROL: u32 = 0x40000107;
    pub const HV_X64_MSR_TSC_EMULATION_STATUS: u32 = 0x40000108;

    /* TSC invariant control */
    pub const HV_X64_MSR_TSC_INVARIANT_CONTROL: u32 = 0x40000118;

    /*
     * The defines related to the synthetic debugger are required by KDNet, but
     * they are not documented in the Hyper-V TLFS because the synthetic debugger
     * functionality has been deprecated and is subject to removal in future
     * versions of Windows.
     */
    pub const HYPERV_CPUID_SYNDBG_VENDOR_AND_MAX_FUNCTIONS: u32 = 0x40000080;
    pub const HYPERV_CPUID_SYNDBG_INTERFACE: u32 = 0x40000081;
    pub const HYPERV_CPUID_SYNDBG_PLATFORM_CAPABILITIES: u32 = 0x40000082;

    /*
     * Hyper-V synthetic debugger platform capabilities
     * These are HYPERV_CPUID_SYNDBG_PLATFORM_CAPABILITIES.EAX bits.
     */
    pub const HV_X64_SYNDBG_CAP_ALLOW_KERNEL_DEBUGGING: u32 = 1 << 1;

    /* Hyper-V Synthetic debug options MSR */
    pub const HV_X64_MSR_SYNDBG_CONTROL: u32 = 0x400000F1;
    pub const HV_X64_MSR_SYNDBG_STATUS: u32 = 0x400000F2;
    pub const HV_X64_MSR_SYNDBG_SEND_BUFFER: u32 = 0x400000F3;
    pub const HV_X64_MSR_SYNDBG_RECV_BUFFER: u32 = 0x400000F4;
    pub const HV_X64_MSR_SYNDBG_PENDING_BUFFER: u32 = 0x400000F5;
    pub const HV_X64_MSR_SYNDBG_OPTIONS: u32 = 0x400000FF;
}

#[allow(dead_code)]
pub mod kvm_msr {
    pub const MSR_KVM_WALL_CLOCK: u32 = 0x11;
    pub const MSR_KVM_SYSTEM_TIME: u32 = 0x12;

    /* Custom MSRs falls in the range 0x4b564d00-0x4b564dff */
    pub const MSR_KVM_WALL_CLOCK_NEW: u32 = 0x4b564d00;
    pub const MSR_KVM_SYSTEM_TIME_NEW: u32 = 0x4b564d01;
    pub const MSR_KVM_ASYNC_PF_EN: u32 = 0x4b564d02;
    pub const MSR_KVM_STEAL_TIME: u32 = 0x4b564d03;
    pub const MSR_KVM_PV_EOI_EN: u32 = 0x4b564d04;
    pub const MSR_KVM_POLL_CONTROL: u32 = 0x4b564d05;
    pub const MSR_KVM_ASYNC_PF_INT: u32 = 0x4b564d06;
    pub const MSR_KVM_ASYNC_PF_ACK: u32 = 0x4b564d07;
    pub const MSR_KVM_MIGRATION_CONTROL: u32 = 0x4b564d08;

    pub const PIN_BASED_ALWAYSON_WITHOUT_TRUE_MSR: u64 = 0x00000016;
    pub const CPU_BASED_ALWAYSON_WITHOUT_TRUE_MSR: u64 = 0x0401e172;
    pub const VM_EXIT_ALWAYSON_WITHOUT_TRUE_MSR: u64 = 0x00036dff;
    pub const VM_ENTRY_ALWAYSON_WITHOUT_TRUE_MSR: u64 = 0x000011ff;
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum VcpuSegment {
    ES,
    CS,
    SS,
    DS,
    FS,
    GS,
    TR,
    LDTR,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum SegmentCacheField {
    SEL = 0,
    BASE = 1,
    LIMIT = 2,
    AR = 3,
    NR = 4,
}
