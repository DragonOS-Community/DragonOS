// use crate::include::bindings::bindings::{phys_2_virt, virt_2_phys};
// use crate::driver::disk::ahci::{ahci_port_base_phys_addr, ahci_port_base_vaddr};
use alloc::vec::Vec;
use core::ops::{BitAnd, BitOr, Not};
use core::{intrinsics::size_of, ptr};

/// 文件说明: 实现了 AHCI 中的控制器 HBA 的相关行为

/// 根据 AHCI 写出 HBA 的 Command
pub const ATA_CMD_READ_DMA_EXT: u8 = 0x25; // 读操作，并且退出
pub const ATA_CMD_WRITE_DMA_EXT: u8 = 0x35; // 写操作，并且退出
pub const ATA_CMD_IDENTIFY: u8 = 0xEC;
pub const ATA_CMD_IDENTIFY_PACKET: u8 = 0xA1;
pub const ATA_CMD_PACKET: u8 = 0xA0;
pub const ATA_DEV_BUSY: u8 = 0x80;
pub const ATA_DEV_DRQ: u8 = 0x08;

/// 都拿来干啥的阿？
pub const HBA_PORT_CMD_CR: u32 = 1 << 15;
pub const HBA_PORT_CMD_FR: u32 = 1 << 14;
pub const HBA_PORT_CMD_FRE: u32 = 1 << 4;
pub const HBA_PORT_CMD_ST: u32 = 1;
pub const HBA_PORT_IS_ERR: u32 = 1 << 30 | 1 << 29 | 1 << 28 | 1 << 27;
pub const HBA_SSTS_PRESENT: u32 = 0x3;
pub const HBA_SIG_ATA: u32 = 0x00000101;
pub const HBA_SIG_ATAPI: u32 = 0xEB140101;
pub const HBA_SIG_PM: u32 = 0x96690101;
pub const HBA_SIG_SEMB: u32 = 0xC33C0101;

/// 接入 Port 的 不同设备类型
#[derive(Debug, Clone, Copy)]
pub enum HbaPortType {
    None,
    Unknown(u32),
    SATA,
    SATAPI,
    PM,
    SEMB,
}

/// 声明了 HBA 的所有属性
#[repr(packed)]
#[derive(Debug, Clone)]
pub struct HbaPort {
    pub clb: _Volatile<u64>,   // 0x00, command list base address, 1K-byte aligned
    pub fb: _Volatile<u64>,    // 0x08, FIS base address, 256-byte aligned
    pub is: _Volatile<u32>,    // 0x10, interrupt status
    pub ie: _Volatile<u32>,    // 0x14, interrupt enable
    pub cmd: _Volatile<u32>,   // 0x18, command and status
    pub _rsv0: _Volatile<u32>, // 0x1C, Reserved
    pub tfd: _Volatile<u32>,   // 0x20, task file data
    pub sig: _Volatile<u32>,   // 0x24, signature
    pub ssts: _Volatile<u32>,  // 0x28, SATA status (SCR0:SStatus)
    pub sctl: _Volatile<u32>,  // 0x2C, SATA control (SCR2:SControl)
    pub serr: _Volatile<u32>,  // 0x30, SATA error (SCR1:SError)
    pub sact: _Volatile<u32>,  // 0x34, SATA active (SCR3:SActive)
    pub ci: _Volatile<u32>,    // 0x38, command issue
    pub sntf: _Volatile<u32>,  // 0x3C, SATA notification (SCR4:SNotification)
    pub fbs: _Volatile<u32>,   // 0x40, FIS-based switch control
    pub _rsv1: [u32; 11],      // 0x44 ~ 0x6F, Reserved
    pub vendor: [u32; 4],      // 0x70 ~ 0x7F, vendor specific
}

#[derive(Debug, Clone, Copy)]
struct _Volatile<T: BitAnd<Output = T> + BitOr<Output = T> + Not<Output = T>> {
    value: T,
}

impl<T: BitAnd<Output = T> + BitOr<Output = T> + Not<Output = T>> _Volatile<T> {
    pub fn new(value: T) -> _Volatile<T> {
        _Volatile { value }
    }
    pub fn read(&self) -> T {
        unsafe { ptr::read_volatile(&self.value) }
    }
    pub fn write(&mut self, new_value: T) {
        unsafe { ptr::write_volatile(&mut self.value, new_value) }
    }
    pub fn set_bit(&mut self, val: T, flag: bool) {
        self.write(match flag {
            true => self.read() | val,
            false => self.read() & (!val),
        });
    }
    pub fn write_bit(&mut self, bits: T, val: T) {
        self.set_bit(bits, false);
        self.set_bit(val, true);
    }
}

/// 全称 HBA Memory Register，是HBA的寄存器在内存中的映射
#[repr(packed)]
#[derive(Debug, Clone)]
pub struct HbaMem {
    pub cap: _Volatile<u32>,     // 0x00, Host capability
    pub ghc: _Volatile<u32>,     // 0x04, Global host control
    pub is: _Volatile<u32>,      // 0x08, Interrupt status
    pub pi: _Volatile<u32>,      // 0x0C, Port implemented
    pub vs: _Volatile<u32>,      // 0x10, Version
    pub ccc_ctl: _Volatile<u32>, // 0x14, Command completion coalescing control
    pub ccc_pts: _Volatile<u32>, // 0x18, Command completion coalescing ports
    pub em_loc: _Volatile<u32>,  // 0x1C, Enclosure management location
    pub em_ctl: _Volatile<u32>,  // 0x20, Enclosure management control
    pub cap2: _Volatile<u32>,    // 0x24, Host capabilities extended
    pub bohc: _Volatile<u32>,    // 0x28, BIOS/OS handoff control and status
    pub _rsv: [u8; 116],         // 0x2C - 0x9F, Reserved
    pub vendor: [u8; 96],        // 0xA0 - 0xFF, Vendor specific registers
    pub ports: [HbaPort; 32],    // 0x100 - 0x10FF, Port control registers
}

/// HBA Command Table 里面的 PRDT 项
/// 作用: 记录了内存中读/写数据的位置，以及长度。你可以把他类比成一个指针？
#[repr(packed)]
#[derive(Debug, Clone, Copy)]
pub struct HbaPrdtEntry {
    pub dba: _Volatile<u64>, // Data base address
    _rsv0: _Volatile<u32>,   // Reserved
    pub dbc: _Volatile<u32>, // Byte count, 4M max, interrupt = 1
}

/// HAB Command Table
/// 每个 Port 一个 Table，主机和设备的交互都靠这个数据结构
#[repr(packed)]
#[derive(Debug, Clone, Copy)]
pub struct HbaCmdTable {
    // 0x00
    pub cfis: [u8; 64], // Command FIS
    // 0x40
    pub acmd: [u8; 16], // ATAPI command, 12 or 16 bytes
    // 0x50
    _rsv: [u8; 48], // Reserved
    // 0x80
    pub prdt_entry: [HbaPrdtEntry; 65535], // Physical region descriptor table entries, 0 ~ 65535
}

/// HBA Command Header
/// 作用: 你可以把他类比成 Command Table 的指针。
/// 猜测: 这里多了一层 Header，而不是直接在 HbaMem 结构体指向 CmdTable，可能是为了兼容和可移植性？
#[repr(packed)]
#[derive(Debug, Clone, Copy)]
pub struct HbaCmdHeader {
    // DW0
    pub cfl: _Volatile<u8>,
    // Command FIS length in DWORDS: 5(len in [2, 16]), atapi: 1, write - host to device: 1, prefetchable: 1
    pub _pm: _Volatile<u8>, // Reset - 0x80, bist: 0x40, clear busy on ok: 0x20, port multiplier
    pub prdtl: _Volatile<u16>, // Physical region descriptor table length in entries
    // DW1
    pub _prdbc: _Volatile<u32>, // Physical region descriptor byte count transferred
    // DW2, 3
    pub ctba: _Volatile<u64>, // Command table descriptor base address
    // DW4 - 7
    pub _rsv1: [u32; 4], // Reserved
}

/// Port 的函数实现
impl HbaPort {
    /// 获取设备类型
    pub fn check_type(&self) -> HbaPortType {
        if self.ssts.read() & HBA_SSTS_PRESENT > 0 {
            let sig = self.sig.read();
            match sig {
                HBA_SIG_ATA => HbaPortType::SATA,
                HBA_SIG_ATAPI => HbaPortType::SATAPI,
                HBA_SIG_PM => HbaPortType::PM,
                HBA_SIG_SEMB => HbaPortType::SEMB,
                _ => HbaPortType::Unknown(sig),
            }
        } else {
            HbaPortType::None
        }
    }

    /// 启动该端口的命令引擎
    pub fn start(&mut self) {
        while self.cmd.read() & HBA_PORT_CMD_CR > 0 {
            core::hint::spin_loop();
        }

        self.cmd
            .write(self.cmd.read() | HBA_PORT_CMD_FRE | HBA_PORT_CMD_ST);
    }

    /// 关闭该端口的命令引擎
    pub fn stop(&mut self) {
        self.cmd
            .write((u32::MAX ^ HBA_PORT_CMD_ST) & self.cmd.read());

        while self.cmd.read() & (HBA_PORT_CMD_FR | HBA_PORT_CMD_CR)
            == (HBA_PORT_CMD_FR | HBA_PORT_CMD_CR)
        {
            core::hint::spin_loop();
        }

        self.cmd
            .write((u32::MAX ^ HBA_PORT_CMD_FRE) & self.cmd.read());
    }

    /// @return: 返回一个空闲 cmd table 的 id; 如果没有，则返回 Option::None
    pub fn find_cmdslot(&self) -> Option<u32> {
        let slots = self.sact.read() | self.ci.read();
        for i in 0..32 {
            if slots & 1 << i == 0 {
                return Some(i);
            }
        }
        None
    }

    /// 初始化,  把 CmdList 等变量的地址赋值到 HbaPort 上 - 这些空间由操作系统分配且固定
    /// 等价于原C版本的 port_rebase 函数
    pub fn init(&mut self, clb: u64, fb: u64, ctbas: &Vec<u64>) {
        self.stop(); // 先暂停端口

        // 赋值 command list base address
        // Command list offset: 1K*portno
        // Command list entry size = 32
        // Command list entry maxim count = 32
        // Command list maxim size = 32*32 = 1K per port
        self.clb.write(clb);
        unsafe {
            ptr::write_bytes(clb as *mut u64, 0, 1024);
        }

        // 赋值 fis base address
        // FIS offset: 32K+256*portno
        // FIS entry size = 256 bytes per port
        self.fb.write(fb);
        unsafe {
            ptr::write_bytes(fb as *mut u64, 0, 256);
        }

        // 赋值 command table base address
        // Command table offset: 40K + 8K*portno
        // Command table size = 256*32 = 8K per port
        let mut cmdheaders = unsafe { clb as *mut HbaCmdHeader };
        for i in 0..32 as usize {
            unsafe {
                (*cmdheaders).prdtl.write(0); // 一开始没有询问，prdtl = 0
                (*cmdheaders).ctba.write(ctbas[i]);
                // 这里限制了 prdtl <= 8, 所以一共用了256bytes，如果需要修改，可以修改这里
                ptr::write_bytes(ctbas[i] as *mut u64, 0, 256);
                cmdheaders = (cmdheaders as usize + size_of::<HbaCmdHeader>()) as *mut HbaCmdHeader;
            }
        }

        // 启动中断
        self.ie.write(0 /*TODO: Enable interrupts: 0b10111*/);

        // 错误码
        self.serr.write(self.serr.read());

        // Disable power management
        self.sctl.write(self.sctl.read() | 7 << 8);

        // Power on and spin up device
        self.cmd.write(self.cmd.read() | 1 << 2 | 1 << 1);

        self.start(); // 重新开启端口
    }
}

#[repr(u8)]
pub enum FisType {
    /// Register FIS - host to device
    RegH2D = 0x27,
    /// Register FIS - device to host
    RegD2H = 0x34,
    /// DMA activate FIS - device to host
    DmaAct = 0x39,
    /// DMA setup FIS - bidirectional
    DmaSetup = 0x41,
    /// Data FIS - bidirectional
    Data = 0x46,
    /// BIST activate FIS - bidirectional
    Bist = 0x58,
    /// PIO setup FIS - device to host
    PioSetup = 0x5F,
    /// Set device bits FIS - device to host
    DevBits = 0xA1,
}

#[repr(packed)]
pub struct FisRegH2D {
    // DWORD 0
    pub fis_type: _Volatile<u8>, // FIS_TYPE_REG_H2D

    pub pm: _Volatile<u8>, // Port multiplier, 1: Command, 0: Control
    // uint8_t pmport : 4; // Port multiplier  低4位
    // uint8_t rsv0 : 3;   // Reserved
    // uint8_t c : 1;      // 1: Command, 0: Control
    
    pub command: _Volatile<u8>,  // Command register
    pub featurel: _Volatile<u8>, // Feature register, 7:0

    // DWORD 1
    pub lba0: _Volatile<u8>,   // LBA low register, 7:0
    pub lba1: _Volatile<u8>,   // LBA mid register, 15:8
    pub lba2: _Volatile<u8>,   // LBA high register, 23:16
    pub device: _Volatile<u8>, // Device register

    // DWORD 2
    pub lba3: _Volatile<u8>,     // LBA register, 31:24
    pub lba4: _Volatile<u8>,     // LBA register, 39:32
    pub lba5: _Volatile<u8>,     // LBA register, 47:40
    pub featureh: _Volatile<u8>, // Feature register, 15:8

    // DWORD 3
    pub countl: _Volatile<u8>,  // Count register, 7:0
    pub counth: _Volatile<u8>,  // Count register, 15:8
    pub icc: _Volatile<u8>,     // Isochronous command completion
    pub control: _Volatile<u8>, // Control register

    // DWORD 4
    pub rsv1: [u8; 4], // Reserved
}

#[repr(packed)]
pub struct FisRegD2H {
    // DWORD 0
    pub fis_type: _Volatile<u8>, // FIS_TYPE_REG_D2H

    pub pm: _Volatile<u8>, // Port multiplier, Interrupt bit: 2

    pub status: _Volatile<u8>, // Status register
    pub error: _Volatile<u8>,  // Error register

    // DWORD 1
    pub lba0: _Volatile<u8>,   // LBA low register, 7:0
    pub lba1: _Volatile<u8>,   // LBA mid register, 15:8
    pub lba2: _Volatile<u8>,   // LBA high register, 23:16
    pub device: _Volatile<u8>, // Device register

    // DWORD 2
    pub lba3: _Volatile<u8>, // LBA register, 31:24
    pub lba4: _Volatile<u8>, // LBA register, 39:32
    pub lba5: _Volatile<u8>, // LBA register, 47:40
    pub rsv2: _Volatile<u8>, // Reserved

    // DWORD 3
    pub countl: _Volatile<u8>, // Count register, 7:0
    pub counth: _Volatile<u8>, // Count register, 15:8
    pub rsv3: [u8; 2],         // Reserved

    // DWORD 4
    pub rsv4: [u8; 4], // Reserved
}

#[repr(packed)]
pub struct FisData {
    // DWORD 0
    pub fis_type: _Volatile<u8>, // FIS_TYPE_DATA

    pub pm: _Volatile<u8>, // Port multiplier

    pub rsv1: [u8; 2], // Reserved

    // DWORD 1 ~ N
    pub data: [u8; 252], // Payload
}

#[repr(packed)]
pub struct FisPioSetup {
    // DWORD 0
    pub fis_type: _Volatile<u8>, // FIS_TYPE_PIO_SETUP

    pub pm: _Volatile<u8>, // Port multiplier, direction: 4 - device to host, interrupt: 2

    pub status: _Volatile<u8>, // Status register
    pub error: _Volatile<u8>,  // Error register

    // DWORD 1
    pub lba0: _Volatile<u8>,   // LBA low register, 7:0
    pub lba1: _Volatile<u8>,   // LBA mid register, 15:8
    pub lba2: _Volatile<u8>,   // LBA high register, 23:16
    pub device: _Volatile<u8>, // Device register

    // DWORD 2
    pub lba3: _Volatile<u8>, // LBA register, 31:24
    pub lba4: _Volatile<u8>, // LBA register, 39:32
    pub lba5: _Volatile<u8>, // LBA register, 47:40
    pub rsv2: _Volatile<u8>, // Reserved

    // DWORD 3
    pub countl: _Volatile<u8>,   // Count register, 7:0
    pub counth: _Volatile<u8>,   // Count register, 15:8
    pub rsv3: _Volatile<u8>,     // Reserved
    pub e_status: _Volatile<u8>, // New value of status register

    // DWORD 4
    pub tc: _Volatile<u16>, // Transfer count
    pub rsv4: [u8; 2],      // Reserved
}

#[repr(packed)]
pub struct FisDmaSetup {
    // DWORD 0
    pub fis_type: _Volatile<u8>, // FIS_TYPE_DMA_SETUP

    pub pm: _Volatile<u8>, // Port multiplier, direction: 4 - device to host, interrupt: 2, auto-activate: 1

    pub rsv1: [u8; 2], // Reserved

    // DWORD 1&2
    pub dma_buffer_id: _Volatile<u64>, /* DMA Buffer Identifier. Used to Identify DMA buffer in host memory. SATA Spec says host specific and not in Spec. Trying AHCI spec might work. */

    // DWORD 3
    pub rsv3: _Volatile<u32>, // More reserved

    // DWORD 4
    pub dma_buffer_offset: _Volatile<u32>, // Byte offset into buffer. First 2 bits must be 0

    // DWORD 5
    pub transfer_count: _Volatile<u32>, // Number of bytes to transfer. Bit 0 must be 0

    // DWORD 6
    pub rsv6: _Volatile<u32>, // Reserved
}
