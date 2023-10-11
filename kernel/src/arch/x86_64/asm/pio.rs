use crate::arch::io::PortIOArch;

pub struct X86_64PortIOArch;

impl PortIOArch for X86_64PortIOArch {
    #[inline(always)]
    unsafe fn in8(port: u16) -> u8 {
        x86::io::inb(port)
    }

    #[inline(always)]
    unsafe fn in16(port: u16) -> u16 {
        x86::io::inw(port)
    }

    #[inline(always)]
    unsafe fn in32(port: u16) -> u32 {
        x86::io::inl(port)
    }

    #[inline(always)]
    unsafe fn out8(port: u16, data: u8) {
        x86::io::outb(port, data)
    }

    #[inline(always)]
    unsafe fn out16(port: u16, data: u16) {
        x86::io::outw(port, data)
    }

    #[inline(always)]
    unsafe fn out32(port: u16, data: u32) {
        x86::io::outl(port, data)
    }
}
