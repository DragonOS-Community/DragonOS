use crate::arch::io::PortIOArch;

pub struct LoongArch64PortIOArch;

impl PortIOArch for LoongArch64PortIOArch {
    #[inline(always)]
    unsafe fn in8(_port: u16) -> u8 {
        unimplemented!("LoongArch64PortIOArch::in8")
    }

    #[inline(always)]
    unsafe fn in16(_port: u16) -> u16 {
        unimplemented!("LoongArch64PortIOArch::in16")
    }

    #[inline(always)]
    unsafe fn in32(_port: u16) -> u32 {
        unimplemented!("LoongArch64PortIOArch::in32")
    }

    #[inline(always)]
    unsafe fn out8(_port: u16, _data: u8) {
        unimplemented!("LoongArch64PortIOArch::out8")
    }

    #[inline(always)]
    unsafe fn out16(_port: u16, _data: u16) {
        unimplemented!("LoongArch64PortIOArch::out16")
    }

    #[inline(always)]
    unsafe fn out32(_port: u16, _data: u32) {
        unimplemented!("LoongArch64PortIOArch::out32")
    }
}
