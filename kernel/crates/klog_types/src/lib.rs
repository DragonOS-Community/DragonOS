#![no_std]
#![feature(const_refs_to_cell)]
#![feature(const_size_of_val)]
#![allow(clippy::needless_return)]

extern crate alloc;
use core::{fmt::Debug, mem::size_of_val};

use alloc::format;
use kdepends::{memoffset::offset_of, thingbuf::StaticThingBuf};

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct AllocatorLog {
    /// 日志的id
    pub id: u64,
    /// 日志类型
    pub type_: AllocatorLogType,
    /// 日志的时间
    pub time: u64,

    /// 日志的来源
    pub source: LogSource,

    /// 日志的来源pid
    pub pid: Option<usize>,

    pub checksum: u64,
}

impl AllocatorLog {
    /// 创建一个日志
    ///
    /// ## 参数
    ///
    /// - `id`：日志的id
    /// - `type_`：日志类型
    /// - `source`：日志来源
    /// - `pid`：日志来源的pid
    /// - `time`：日志的时间
    pub fn new(
        id: u64,
        type_: AllocatorLogType,
        source: LogSource,
        pid: Option<usize>,
        time: u64,
    ) -> Self {
        let mut x = Self {
            id,
            type_,
            time,
            source,
            pid,
            checksum: 0,
        };
        let checksum = Self::calculate_checksum(&x);
        x.checksum = checksum;
        return x;
    }

    pub const fn zeroed() -> Self {
        return Self {
            id: 0,
            type_: AllocatorLogType::Undefined,
            time: 0,
            source: LogSource::Undefined,
            pid: None,
            checksum: 0,
        };
    }

    /// 计算日志的校验和
    pub fn calculate_checksum(value: &Self) -> u64 {
        let buf = unsafe {
            core::slice::from_raw_parts(
                value as *const _ as *const u8,
                core::mem::size_of::<Self>() - core::mem::size_of::<u64>(),
            )
        };
        let checksum = kdepends::crc::crc64::crc64_be(0, buf);
        return checksum;
    }

    /// 验证日志的校验和
    pub fn validate_checksum(&self) -> bool {
        let checksum = Self::calculate_checksum(self);
        return checksum == self.checksum;
    }

    /// 当前日志是否有效
    pub fn is_valid(&self) -> bool {
        if !self.validate_checksum() {
            return false;
        }

        if self.id == 0 {
            return false;
        }

        return true;
    }
}

impl PartialOrd for AllocatorLog {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for AllocatorLog {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        return self.id.cmp(&other.id);
    }
}

/// 内存分配器日志类型
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum AllocatorLogType {
    Undefined,
    Alloc(AllocLogItem),
    AllocZeroed(AllocLogItem),
    Free(AllocLogItem),
}

#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct AllocLogItem {
    pub layout: core::alloc::Layout,
    pub vaddr: Option<usize>,
    pub paddr: Option<usize>,
}

impl AllocLogItem {
    pub fn new(layout: core::alloc::Layout, vaddr: Option<usize>, paddr: Option<usize>) -> Self {
        return Self {
            layout,
            vaddr,
            paddr,
        };
    }
}

impl Debug for AllocLogItem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AllocLogItem")
            .field("layout", &self.layout)
            .field(
                "vaddr",
                &format_args!("{:#x}", *self.vaddr.as_ref().unwrap_or(&0)),
            )
            .field(
                "paddr",
                &format_args!("{:#x}", self.paddr.as_ref().unwrap_or(&0)),
            )
            .finish()
    }
}

#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum LogSource {
    Undefined = 0,
    Bump = 1,
    Buddy = 2,
    Slab = 3,
}

pub struct MMLogCycle;

impl MMLogCycle {
    pub const fn new() -> Self {
        Self {}
    }
}

impl kdepends::thingbuf::Recycle<AllocatorLog> for MMLogCycle {
    fn new_element(&self) -> AllocatorLog {
        AllocatorLog::zeroed()
    }

    fn recycle(&self, element: &mut AllocatorLog) {
        *element = AllocatorLog::zeroed();
    }
}

/// 内存分配器日志通道
#[repr(C)]
pub struct MMLogChannel<const CAP: usize> {
    pub magic: u32,
    /// 日志元素的大小
    pub element_size: u32,
    /// 日志通道每个槽的大小（字节）
    pub slot_size: u32,
    pub capacity: u64,
    pub slots_offset: u64,
    pub buf: StaticThingBuf<AllocatorLog, CAP, MMLogCycle>,
}

impl<const CAP: usize> Debug for MMLogChannel<CAP> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MMLogChannel")
            .field("magic", &format!("{:#x}", self.magic))
            .field("element_size", &self.element_size)
            .field("capacity", &self.capacity)
            .field("slots_offset", &self.slots_offset)
            .field(
                "buf",
                &format!(
                    "StaticThingBuf<AllocatorLog, {}, MMLogCycle>",
                    self.capacity
                ),
            )
            .finish()
    }
}

impl<const CAP: usize> MMLogChannel<CAP> {
    /// 日志通道的魔数
    pub const MM_LOG_CHANNEL_MAGIC: u32 = 0x4d4c4348;

    /// 创建一个大小为`capacity`日志通道
    pub const fn new(capacity: usize) -> Self {
        let buffer = StaticThingBuf::with_recycle(MMLogCycle::new());
        assert!(buffer.offset_of_slots() != 0);
        let slot_total_size = size_of_val(&buffer) - buffer.offset_of_slots();
        let slot_size = slot_total_size / capacity;
        assert!(slot_size != 0);
        assert!(slot_size > size_of_val(&AllocatorLog::zeroed()));

        let r = Self {
            magic: Self::MM_LOG_CHANNEL_MAGIC,
            element_size: core::mem::size_of::<AllocatorLog>() as u32,
            capacity: capacity as u64,
            slot_size: slot_size as u32,
            slots_offset: (offset_of!(MMLogChannel<CAP>, buf) + buffer.offset_of_slots()) as u64,
            buf: buffer,
        };

        return r;
    }
}
