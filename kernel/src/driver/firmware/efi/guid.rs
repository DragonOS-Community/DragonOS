use core::{fmt, mem};

use uefi_raw::Guid;

/// 由D人agonStub设置的，用于描述内核被放置在的地址的GUID
pub static DRAGONSTUB_EFI_PAYLOAD_EFI_GUID: Guid = Guid::new(
    unsafe { mem::transmute_copy(&0xddf1d47cu32) },
    unsafe { mem::transmute_copy(&0x102cu32) },
    unsafe { mem::transmute_copy(&0xaaf9u32) },
    0xce,
    0x34,
    [0xbc, 0xef, 0x98, 0x12, 0x00, 0x31],
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
