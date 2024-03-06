use core::{
    cell::RefCell,
    hint::spin_loop,
    ptr::{read_volatile, write_volatile},
};

use crate::{
    kdebug, kerror, kinfo,
    mm::{
        mmio_buddy::{mmio_pool, MMIOSpaceGuard},
        percpu::PerCpu,
        PhysAddr, VirtAddr,
    },
    smp::core::smp_get_processor_id,
};

use super::{hw_irq::ApicId, LVTRegister, LocalAPIC, LVT};

/// per-cpu的xAPIC的MMIO空间起始地址
static mut XAPIC_INSTANCES: [RefCell<Option<XApic>>; PerCpu::MAX_CPU_NUM as usize] =
    [const { RefCell::new(None) }; PerCpu::MAX_CPU_NUM as usize];

#[inline(always)]
pub(super) fn current_xapic_instance() -> &'static RefCell<Option<XApic>> {
    unsafe { &XAPIC_INSTANCES.as_ref()[smp_get_processor_id().data() as usize] }
}

/// TODO：统一变量
/// @brief local APIC 寄存器地址偏移量
#[derive(Debug)]
#[allow(dead_code)]
#[allow(non_camel_case_types)]
#[repr(u32)]
pub enum XApicOffset {
    // 定义各个寄存器的地址偏移量
    LOCAL_APIC_OFFSET_Local_APIC_ID = 0x20,
    LOCAL_APIC_OFFSET_Local_APIC_Version = 0x30,
    LOCAL_APIC_OFFSET_Local_APIC_TPR = 0x80,
    LOCAL_APIC_OFFSET_Local_APIC_APR = 0x90,
    LOCAL_APIC_OFFSET_Local_APIC_PPR = 0xa0,
    LOCAL_APIC_OFFSET_Local_APIC_EOI = 0xb0,
    LOCAL_APIC_OFFSET_Local_APIC_RRD = 0xc0,
    LOCAL_APIC_OFFSET_Local_APIC_LDR = 0xd0,
    LOCAL_APIC_OFFSET_Local_APIC_DFR = 0xe0,
    LOCAL_APIC_OFFSET_Local_APIC_SVR = 0xf0,

    LOCAL_APIC_OFFSET_Local_APIC_ISR_31_0 = 0x100, // In-Service Register
    LOCAL_APIC_OFFSET_Local_APIC_ISR_63_32 = 0x110,
    LOCAL_APIC_OFFSET_Local_APIC_ISR_95_64 = 0x120,
    LOCAL_APIC_OFFSET_Local_APIC_ISR_127_96 = 0x130,
    LOCAL_APIC_OFFSET_Local_APIC_ISR_159_128 = 0x140,
    LOCAL_APIC_OFFSET_Local_APIC_ISR_191_160 = 0x150,
    LOCAL_APIC_OFFSET_Local_APIC_ISR_223_192 = 0x160,
    LOCAL_APIC_OFFSET_Local_APIC_ISR_255_224 = 0x170,

    LOCAL_APIC_OFFSET_Local_APIC_TMR_31_0 = 0x180, // Trigger Mode Register
    LOCAL_APIC_OFFSET_Local_APIC_TMR_63_32 = 0x190,
    LOCAL_APIC_OFFSET_Local_APIC_TMR_95_64 = 0x1a0,
    LOCAL_APIC_OFFSET_Local_APIC_TMR_127_96 = 0x1b0,
    LOCAL_APIC_OFFSET_Local_APIC_TMR_159_128 = 0x1c0,
    LOCAL_APIC_OFFSET_Local_APIC_TMR_191_160 = 0x1d0,
    LOCAL_APIC_OFFSET_Local_APIC_TMR_223_192 = 0x1e0,
    LOCAL_APIC_OFFSET_Local_APIC_TMR_255_224 = 0x1f0,

    LOCAL_APIC_OFFSET_Local_APIC_IRR_31_0 = 0x200, // Interrupt Request Register
    LOCAL_APIC_OFFSET_Local_APIC_IRR_63_32 = 0x210,
    LOCAL_APIC_OFFSET_Local_APIC_IRR_95_64 = 0x220,
    LOCAL_APIC_OFFSET_Local_APIC_IRR_127_96 = 0x230,
    LOCAL_APIC_OFFSET_Local_APIC_IRR_159_128 = 0x240,
    LOCAL_APIC_OFFSET_Local_APIC_IRR_191_160 = 0x250,
    LOCAL_APIC_OFFSET_Local_APIC_IRR_223_192 = 0x260,
    LOCAL_APIC_OFFSET_Local_APIC_IRR_255_224 = 0x270,

    LOCAL_APIC_OFFSET_Local_APIC_ESR = 0x280, // Error Status Register

    LOCAL_APIC_OFFSET_Local_APIC_LVT_CMCI = 0x2f0, // Corrected Machine Check Interrupt Register

    LOCAL_APIC_OFFSET_Local_APIC_ICR_31_0 = 0x300, // Interrupt Command Register
    LOCAL_APIC_OFFSET_Local_APIC_ICR_63_32 = 0x310,

    LOCAL_APIC_OFFSET_Local_APIC_LVT_TIMER = 0x320,
    LOCAL_APIC_OFFSET_Local_APIC_LVT_THERMAL = 0x330,
    LOCAL_APIC_OFFSET_Local_APIC_LVT_PERFORMANCE_MONITOR = 0x340,
    LOCAL_APIC_OFFSET_Local_APIC_LVT_LINT0 = 0x350,
    LOCAL_APIC_OFFSET_Local_APIC_LVT_LINT1 = 0x360,
    LOCAL_APIC_OFFSET_Local_APIC_LVT_ERROR = 0x370,
    // 初始计数寄存器（定时器专用）
    LOCAL_APIC_OFFSET_Local_APIC_INITIAL_COUNT_REG = 0x380,
    // 当前计数寄存器（定时器专用）
    LOCAL_APIC_OFFSET_Local_APIC_CURRENT_COUNT_REG = 0x390,
    LOCAL_APIC_OFFSET_Local_APIC_CLKDIV = 0x3e0,
}

impl Into<u32> for XApicOffset {
    fn into(self) -> u32 {
        self as u32
    }
}

impl From<LVTRegister> for XApicOffset {
    fn from(lvt: LVTRegister) -> Self {
        match lvt {
            LVTRegister::Timer => XApicOffset::LOCAL_APIC_OFFSET_Local_APIC_LVT_TIMER,
            LVTRegister::Thermal => XApicOffset::LOCAL_APIC_OFFSET_Local_APIC_LVT_THERMAL,
            LVTRegister::PerformanceMonitor => {
                XApicOffset::LOCAL_APIC_OFFSET_Local_APIC_LVT_PERFORMANCE_MONITOR
            }
            LVTRegister::LINT0 => XApicOffset::LOCAL_APIC_OFFSET_Local_APIC_LVT_LINT0,
            LVTRegister::LINT1 => XApicOffset::LOCAL_APIC_OFFSET_Local_APIC_LVT_LINT1,
            LVTRegister::ErrorReg => XApicOffset::LOCAL_APIC_OFFSET_Local_APIC_LVT_ERROR,
            LVTRegister::CMCI => XApicOffset::LOCAL_APIC_OFFSET_Local_APIC_LVT_CMCI,
        }
    }
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct XApic {
    /// 当前xAPIC的寄存器映射的虚拟地址。注意，每个CPU都有自己的xAPIC，所以这个地址是每个CPU都不一样的。
    apic_vaddr: VirtAddr,
    /// `apic_vaddr`与映射的空间起始位置之间的偏移量
    offset: usize,
    map_guard: MMIOSpaceGuard,
    xapic_base: PhysAddr,
}

impl XApic {
    /// 读取指定寄存器的值
    #[allow(dead_code)]
    pub unsafe fn read(&self, reg: XApicOffset) -> u32 {
        read_volatile((self.apic_vaddr.data() + reg as usize) as *const u32)
    }

    /// 将指定的值写入寄存器
    #[allow(dead_code)]
    pub unsafe fn write(&self, reg: XApicOffset, value: u32) {
        write_volatile(
            (self.apic_vaddr.data() + (reg as u32) as usize) as *mut u32,
            value,
        );
        self.read(XApicOffset::LOCAL_APIC_OFFSET_Local_APIC_ID); // 等待写操作完成，通过读取进行同步
    }
}

impl XApic {
    /// 创建新的XAPIC实例
    ///
    /// ## 参数
    ///
    /// - `xapic_base` - 当前核心的xAPIC的寄存器的物理地址
    pub unsafe fn new(xapic_base: PhysAddr) -> Self {
        let offset = xapic_base.data() & 0xffff;
        let paddr = PhysAddr::new(xapic_base.data() & !0xffff);
        let g = mmio_pool()
            .create_mmio(4096)
            .expect("Fail to create MMIO for XAPIC");
        g.map_phys(paddr, 4096).expect("Fail to map MMIO for XAPIC");
        let addr = g.vaddr() + offset;

        kdebug!(
            "XAPIC: {:#x} -> {:#x}, offset={offset}",
            xapic_base.data(),
            addr.data()
        );

        let r = Self {
            apic_vaddr: addr,
            offset,
            map_guard: g,
            xapic_base,
        };

        return r;
    }
}

#[allow(dead_code)]
const X1: u32 = 0x0000000B; // 将除数设置为1，即不除频率
#[allow(dead_code)]
const PERIODIC: u32 = 0x00020000; // 周期性模式
#[allow(dead_code)]
const ENABLE: u32 = 0x00000100; // 单元使能
#[allow(dead_code)]
const MASKED: u32 = 0x00010000; // 中断屏蔽
const LEVEL: u32 = 0x00008000; // 电平触发
const BCAST: u32 = 0x00080000; // 发送到所有APIC，包括自己
const DELIVS: u32 = 0x00001000; // 传递状态
const INIT: u32 = 0x00000500; // INIT/RESET

//中断请求
#[allow(dead_code)]
const T_IRQ0: u32 = 32; // IRQ 0 对应于 T_IRQ 中断
#[allow(dead_code)]
const IRQ_TIMER: u32 = 0;
#[allow(dead_code)]
const IRQ_KBD: u32 = 1;
#[allow(dead_code)]
const IRQ_COM1: u32 = 4;
#[allow(dead_code)]
const IRQ_IDE: u32 = 14;
#[allow(dead_code)]
const IRQ_ERROR: u32 = 19;
#[allow(dead_code)]
const IRQ_SPURIOUS: u32 = 31;

impl LocalAPIC for XApic {
    /// @brief 判断处理器是否支持apic
    fn support() -> bool {
        return x86::cpuid::CpuId::new()
            .get_feature_info()
            .expect("Fail to get CPU feature.")
            .has_apic();
    }

    /// @return true -> 函数运行成功
    fn init_current_cpu(&mut self) -> bool {
        unsafe {
            // enable xapic
            x86::msr::wrmsr(x86::msr::APIC_BASE, (self.xapic_base.data() | 0x800) as u64);
            let val = x86::msr::rdmsr(x86::msr::APIC_BASE);
            if val & 0x800 != 0x800 {
                kerror!("xAPIC enable failed: APIC_BASE & 0x800 != 0x800");
                return false;
            }
            // 设置 Spurious Interrupt Vector Register
            let val = self.read(XApicOffset::LOCAL_APIC_OFFSET_Local_APIC_SVR.into());

            self.write(
                XApicOffset::LOCAL_APIC_OFFSET_Local_APIC_SVR.into(),
                val | ENABLE,
            );

            let val = self.read(XApicOffset::LOCAL_APIC_OFFSET_Local_APIC_SVR.into());
            if val & ENABLE == 0 {
                kerror!("xAPIC software enable failed.");

                return false;
            } else {
                kinfo!("xAPIC software enabled.");
            }

            if val & 0x1000 != 0 {
                kinfo!("xAPIC EOI broadcast suppression enabled.");
            }

            self.mask_all_lvt();

            // 清除错误状态寄存器（需要连续写入两次）
            self.write(XApicOffset::LOCAL_APIC_OFFSET_Local_APIC_ESR.into(), 0);
            self.write(XApicOffset::LOCAL_APIC_OFFSET_Local_APIC_ESR.into(), 0);

            // 确认任何未完成的中断
            self.write(XApicOffset::LOCAL_APIC_OFFSET_Local_APIC_EOI.into(), 0);

            // 发送 Init Level De-Assert 信号以同步仲裁ID
            self.write(
                XApicOffset::LOCAL_APIC_OFFSET_Local_APIC_ICR_63_32.into(),
                0,
            );
            self.write(
                XApicOffset::LOCAL_APIC_OFFSET_Local_APIC_ICR_31_0.into(),
                BCAST | INIT | LEVEL,
            );
            while self.read(XApicOffset::LOCAL_APIC_OFFSET_Local_APIC_ICR_31_0.into()) & DELIVS != 0
            {
                spin_loop();
            }
        }

        true
    }

    /// 发送 EOI（End Of Interrupt）
    fn send_eoi(&self) {
        unsafe {
            let s = self as *const Self as *mut Self;
            (*s).write(XApicOffset::LOCAL_APIC_OFFSET_Local_APIC_EOI.into(), 0);
        }
    }

    /// 获取版本号
    fn version(&self) -> u8 {
        unsafe {
            (self.read(XApicOffset::LOCAL_APIC_OFFSET_Local_APIC_Version.into()) & 0xff) as u8
        }
    }

    fn support_eoi_broadcast_suppression(&self) -> bool {
        unsafe {
            ((self.read(XApicOffset::LOCAL_APIC_OFFSET_Local_APIC_Version.into()) >> 24) & 1) == 1
        }
    }

    fn max_lvt_entry(&self) -> u8 {
        unsafe {
            ((self.read(XApicOffset::LOCAL_APIC_OFFSET_Local_APIC_Version.into()) >> 16) & 0xff)
                as u8
                + 1
        }
    }

    /// 获取ID
    fn id(&self) -> ApicId {
        unsafe { ApicId::new(self.read(XApicOffset::LOCAL_APIC_OFFSET_Local_APIC_ID.into()) >> 24) }
    }

    /// 设置LVT寄存器的值
    fn set_lvt(&mut self, lvt: LVT) {
        unsafe {
            self.write(lvt.register().into(), lvt.data);
        }
    }

    fn read_lvt(&self, reg: LVTRegister) -> LVT {
        unsafe {
            LVT::new(
                reg,
                self.read(XApicOffset::LOCAL_APIC_OFFSET_Local_APIC_LVT_TIMER.into()),
            )
            .unwrap()
        }
    }

    fn mask_all_lvt(&mut self) {
        // self.set_lvt(LVT::new(LVTRegister::CMCI, LVT::MASKED).unwrap());
        self.set_lvt(LVT::new(LVTRegister::Timer, LVT::MASKED).unwrap());
        self.set_lvt(LVT::new(LVTRegister::Thermal, LVT::MASKED).unwrap());
        self.set_lvt(LVT::new(LVTRegister::PerformanceMonitor, LVT::MASKED).unwrap());
        self.set_lvt(LVT::new(LVTRegister::LINT0, LVT::MASKED).unwrap());
        self.set_lvt(LVT::new(LVTRegister::LINT1, LVT::MASKED).unwrap());
        self.set_lvt(LVT::new(LVTRegister::ErrorReg, LVT::MASKED).unwrap());
    }

    fn write_icr(&self, icr: x86::apic::Icr) {
        unsafe {
            // Wait for any previous send to finish
            while self.read(XApicOffset::LOCAL_APIC_OFFSET_Local_APIC_ICR_31_0.into()) & DELIVS != 0
            {
                spin_loop();
            }

            self.write(
                XApicOffset::LOCAL_APIC_OFFSET_Local_APIC_ICR_63_32.into(),
                icr.upper(),
            );
            self.write(
                XApicOffset::LOCAL_APIC_OFFSET_Local_APIC_ICR_31_0.into(),
                icr.lower(),
            );

            // Wait for send to finish
            while self.read(XApicOffset::LOCAL_APIC_OFFSET_Local_APIC_ICR_31_0.into()) & DELIVS != 0
            {
                spin_loop();
            }
        }
    }
}
