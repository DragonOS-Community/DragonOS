use core::{fmt, mem};

use uefi_raw::Guid;

/// 由DragonStub设置的，用于描述内核被放置在的地址的GUID
pub static DRAGONSTUB_EFI_PAYLOAD_EFI_GUID: Guid = Guid::new(
    unsafe { mem::transmute_copy(&0xddf1d47cu32) },
    unsafe { mem::transmute_copy(&0x102cu32) },
    unsafe { mem::transmute_copy(&0xaaf9u32) },
    0xce,
    0x34,
    [0xbc, 0xef, 0x98, 0x12, 0x00, 0x31],
);

pub static EFI_MEMORY_ATTRIBUTES_TABLE_GUID: Guid = Guid::new(
    unsafe { mem::transmute_copy(&0xdcfa911du32) },
    unsafe { mem::transmute_copy(&0x26ebu32) },
    unsafe { mem::transmute_copy(&0x469fu32) },
    0xa2,
    0x20,
    [0x38, 0xb7, 0xdc, 0x46, 0x12, 0x20],
);

pub static EFI_MEMRESERVE_TABLE_GUID: Guid = Guid::new(
    unsafe { mem::transmute_copy(&0x888eb0c6u32) },
    unsafe { mem::transmute_copy(&0x8edeu32) },
    unsafe { mem::transmute_copy(&0x4ff5u32) },
    0xa8,
    0xf0,
    [0x9a, 0xee, 0x5c, 0xb9, 0x77, 0xc2],
);

pub static EFI_SYSTEM_RESOURCE_TABLE_GUID: Guid = Guid::new(
    unsafe { mem::transmute_copy(&0xb122a263u32) },
    unsafe { mem::transmute_copy(&0x3661u32) },
    unsafe { mem::transmute_copy(&0x4f68u32) },
    0x99,
    0x29,
    [0x78, 0xf8, 0xb0, 0xd6, 0x21, 0x80],
);
/// 表示内核被加载到的地址的信息。
///
/// 对应 `DRAGONSTUB_EFI_PAYLOAD_EFI_GUID`
#[derive(Clone, Copy)]
#[repr(C)]
pub struct DragonStubPayloadEFI {
    /// 内核文件被加载到的物理地址
    pub paddr: u64,

    /// 占用的空间的大小
    pub size: u64,
}

impl fmt::Debug for DragonStubPayloadEFI {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DragonStubPayloadEFI")
            .field("paddr", &format_args!("0x{:x}", self.paddr))
            .field("size", &self.size)
            .finish()
    }
}
