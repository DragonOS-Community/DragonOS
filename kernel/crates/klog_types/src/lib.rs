#![no_std]
#![feature(const_refs_to_cell)]

extern crate alloc;
use core::fmt::Debug;

use alloc::format;
use kdepends::{memoffset::offset_of, thingbuf::StaticThingBuf};

#[repr(C)]
#[derive(Debug, Copy, Clone)]
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
    pub const fn new(
        id: u64,
        type_: AllocatorLogType,
        source: LogSource,
        pid: Option<usize>,
        time: u64,
    ) -> Self {
        return Self {
            id,
            type_,
            time,
            source,
            pid,
        };
    }

    pub const fn zeroed() -> Self {
        return Self {
            id: 0,
            type_: AllocatorLogType::Undefined,
            time: 0,
            source: LogSource::Undefined,
            pid: None,
        };
    }
}

/// 内存分配器日志类型
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub enum AllocatorLogType {
    Undefined,
    Alloc(AllocLogItem),
    AllocZeroed(AllocLogItem),
    Free(AllocLogItem),
}

#[repr(C)]
#[derive(Copy, Clone)]
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
#[derive(Debug, Copy, Clone)]
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

#[repr(C)]
pub struct MMLogChannel<const CAP: usize> {
    pub magic: u32,
    pub element_size: u32,
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
    pub const MM_LOG_CHANNEL_MAGIC: u32 = 0x4d4c4348;

    pub const fn new(capacity: usize) -> Self {
        let buffer = StaticThingBuf::with_recycle(MMLogCycle::new());
        assert!(buffer.offset_of_slots() != 0);

        let r = Self {
            magic: Self::MM_LOG_CHANNEL_MAGIC,
            element_size: core::mem::size_of::<AllocatorLog>() as u32,
            capacity: capacity as u64,
            slots_offset: (offset_of!(MMLogChannel<CAP>, buf) + buffer.offset_of_slots()) as u64,
            buf: buffer,
        };

        return r;
    }
}
