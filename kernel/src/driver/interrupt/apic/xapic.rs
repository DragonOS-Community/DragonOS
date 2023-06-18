use core::ptr::{read_volatile, write_volatile};

use super::{LVTRegister, LocalAPIC, LVT};

/// @brief local APIC 寄存器地址偏移量
#[derive(Debug)]
#[allow(dead_code)]
#[allow(non_camel_case_types)]
#[repr(u32)]
pub enum LocalApicOffset {
    Local_Apic_ID_Register = 0x20,
    Local_Apic_Version_Register = 0x30,
    Task_Priority_Register = 0x80,
    Arbitration_Priority_Register = 0x90,
    Processor_Priority_Register = 0xa0,
    End_of_Interrupt_Register = 0xb0,
    Remote_Read_Register = 0xc0,
    Logical_Destination_Register = 0xd0,
    Destination_Format_Register = 0xe0,
    Spurious_Interrupt_Vector_Register = 0xf0,

    ISR_31_0 = 0x100, // In-Service Register
    ISR_63_32 = 0x110,
    ISR_95_64 = 0x120,
    ISR_127_96 = 0x130,
    ISR_159_128 = 0x140,
    ISR_191_160 = 0x150,
    ISR_223_192 = 0x160,
    ISR_255_224 = 0x170,

    TMR_31_0 = 0x180, // Trigger Mode Register
    TMR_63_32 = 0x190,
    TMR_95_64 = 0x1a0,
    TMR_127_96 = 0x1b0,
    TMR_159_128 = 0x1c0,
    TMR_191_160 = 0x1d0,
    TMR_223_192 = 0x1e0,
    TMR_255_224 = 0x1f0,

    IRR_31_0 = 0x200, // Interrupt Request Register
    IRR_63_32 = 0x210,
    IRR_95_64 = 0x220,
    IRR_127_96 = 0x230,
    IRR_159_128 = 0x240,
    IRR_191_160 = 0x250,
    IRR_223_192 = 0x260,
    IRR_255_224 = 0x270,

    Error_Status_Register = 0x280,

    LVT_CMCI = 0x2f0, // Corrected Machine Check Interrupt Register

    ICR_31_0 = 0x300, // Interrupt Command Register
    ICR_63_32 = 0x310,

    LVT_Timer_Register = 0x320,
    LVT_Thermal_Sensor_Register = 0x330,
    LVT_Performance_Monitor = 0x340,
    LVT_LINT0_Register = 0x350,
    LVT_LINT1_Register = 0x360,
    LVT_Error_Register = 0x370,
    // 初始计数寄存器（定时器专用）
    Initial_Count_Register = 0x380,
    // 当前计数寄存器（定时器专用）
    Current_Count_Register = 0x390,
    Divide_Configuration_Register = 0x3e0,
}

impl Into<u32> for LocalApicOffset {
    fn into(self) -> u32 {
        self as u32
    }
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct XApic {
    /// 当前xAPIC的MMIO空间起始地址。注意，每个CPU都有自己的xAPIC，所以这个地址是每个CPU都不一样的。
    map_vaddr: usize,
}

impl XApic {
    #[allow(dead_code)]
    unsafe fn read(&self, reg: u32) -> u32 {
        read_volatile((self.map_vaddr + reg as usize) as *const u32)
    }

    #[allow(dead_code)]
    unsafe fn write(&mut self, reg: u32, value: u32) {
        write_volatile((self.map_vaddr + reg as usize) as *mut u32, value);
        self.read(0x20); // wait for write to finish, by reading
    }
}

impl XApic {
    /// Create a new XAPIC.
    ///
    /// # Safety
    ///
    /// You must provide a valid address.
    #[allow(dead_code)]
    pub unsafe fn new(addr: usize) -> Self {
        XApic { map_vaddr: addr }
    }
}
const X1: u32 = 0x0000000B; // divide counts by 1
const PERIODIC: u32 = 0x00020000; // Periodic
const ENABLE: u32 = 0x00000100; // Unit Enable
const MASKED: u32 = 0x00010000; // Interrupt masked
const PCINT: u32 = 0x0340; // Performance Counter LVT
const LEVEL: u32 = 0x00008000; // Level triggered
const BCAST: u32 = 0x00080000; // Send to all APICs, including self.
const DELIVS: u32 = 0x00001000; // Delivery status
const INIT: u32 = 0x00000500; // INIT/RESET

const T_IRQ0: u32 = 32; // IRQ 0 corresponds to int T_IRQ
const IRQ_TIMER: u32 = 0;
const IRQ_KBD: u32 = 1;
const IRQ_COM1: u32 = 4;
const IRQ_IDE: u32 = 14;
const IRQ_ERROR: u32 = 19;
const IRQ_SPURIOUS: u32 = 31;

impl LocalAPIC for XApic {
    /// @brief 判断处理器是否支持apic
    fn support() -> bool {
        return x86::cpuid::CpuId::new()
            .get_feature_info()
            .expect("Get cpu feature info failed.")
            .has_apic();
    }

    /// @return true -> the function works
    fn init_current_cpu(&mut self) -> bool {
        unsafe {
            self.write(
                LocalApicOffset::Spurious_Interrupt_Vector_Register.into(),
                ENABLE | (T_IRQ0 + IRQ_SPURIOUS),
            );

            // The timer repeatedly counts down at bus frequency
            // from lapic[TICR] and then issues an interrupt.
            // If xv6 cared more about precise timekeeping,
            // TICR would be calibrated using an external time source.
            self.write(LocalApicOffset::Divide_Configuration_Register.into(), X1);
            self.write(
                LocalApicOffset::LVT_Timer_Register.into(),
                PERIODIC | (T_IRQ0 + IRQ_TIMER),
            );
            self.write(LocalApicOffset::Initial_Count_Register.into(), 10000000);

            // Disable logical interrupt lines.
            self.write(LocalApicOffset::LVT_LINT0_Register.into(), MASKED);
            self.write(LocalApicOffset::LVT_LINT1_Register.into(), MASKED);

            // Disable performance counter overflow interrupts
            // on machines that provide that interrupt entry.
            if (self.read(LocalApicOffset::Local_Apic_Version_Register.into()) >> 16 & 0xFF) >= 4 {
                self.write(PCINT, MASKED);
            }

            // Map error interrupt to IRQ_ERROR.
            self.write(
                LocalApicOffset::LVT_Error_Register.into(),
                T_IRQ0 + IRQ_ERROR,
            );

            // Clear error status register (requires back-to-back writes).
            self.write(LocalApicOffset::Error_Status_Register.into(), 0);
            self.write(LocalApicOffset::Error_Status_Register.into(), 0);

            // Ack any outstanding interrupts.
            self.write(LocalApicOffset::End_of_Interrupt_Register.into(), 0);

            // Send an Init Level De-Assert to synchronise arbitration ID's.
            self.write(LocalApicOffset::ICR_63_32.into(), 0);
            self.write(LocalApicOffset::ICR_31_0.into(), BCAST | INIT | LEVEL);
            while self.read(LocalApicOffset::ICR_31_0.into()) & DELIVS != 0 {}

            // Enable interrupts on the APIC (but not on the processor).
            self.write(LocalApicOffset::Task_Priority_Register.into(), 0);
        }

        true
    }

    fn send_eoi(&mut self) {
        unsafe {
            self.write(LocalApicOffset::End_of_Interrupt_Register.into(), 0);
        }
    }

    fn version(&self) -> u32 {
        unsafe { self.read(LocalApicOffset::Local_Apic_Version_Register.into()) }
    }

    fn id(&self) -> u32 {
        unsafe { self.read(LocalApicOffset::Local_Apic_ID_Register.into()) >> 24 }
    }

    fn set_lvt(&mut self, register: LVTRegister, lvt: LVT) {
        unsafe {
            self.write(register.into(), lvt.data);
        }
    }
}
