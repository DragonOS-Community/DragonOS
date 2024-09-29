pub const APIC_DEFAULT_PHYS_BASE: u64 = 0xfee00000;
#[allow(dead_code)]
pub const MSR_IA32_APICBASE: u64 = 0x0000001b;
pub const MSR_IA32_APICBASE_BSP: u64 = 1 << 8;
pub const MSR_IA32_APICBASE_ENABLE: u64 = 1 << 11;
#[allow(dead_code)]
pub const MSR_IA32_APICBASE_BASE: u64 = 0xfffff << 12;

pub const APIC_BASE_MSR: u32 = 0x800;
pub const APIC_ID: u32 = 0x20;
pub const APIC_LVR: u32 = 0x30;
pub const APIC_TASKPRI: u32 = 0x80;
pub const APIC_PROCPRI: u32 = 0xA0;
pub const APIC_EOI: u32 = 0xB0;
pub const APIC_SPIV: u32 = 0xF0;
pub const APIC_IRR: u32 = 0x200;
pub const APIC_ICR: u32 = 0x300;
pub const APIC_LVTCMCI: u32 = 0x2f0;
