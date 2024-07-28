/// 每个架构都需要实现的IO接口
#[allow(unused)]
pub trait PortIOArch {
    unsafe fn in8(port: u16) -> u8;
    unsafe fn in16(port: u16) -> u16;
    unsafe fn in32(port: u16) -> u32;
    unsafe fn out8(port: u16, data: u8);
    unsafe fn out16(port: u16, data: u16);
    unsafe fn out32(port: u16, data: u32);
}
