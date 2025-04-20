bitflags! {
    pub struct GRandFlags: u8{
        const GRND_NONBLOCK = 0x0001;
        const GRND_RANDOM = 0x0002;
        const GRND_INSECURE = 0x0004;
    }
}

// 软件实现的随机数生成器
#[allow(dead_code)]
pub fn soft_rand() -> usize {
    static mut SEED: u64 = 0xdead_beef_cafe_babe;
    let mut buf = [0u8; size_of::<usize>()];
    for x in buf.iter_mut() {
        unsafe {
            // from musl
            SEED = SEED.wrapping_mul(0x5851_f42d_4c95_7f2d);
            *x = (SEED >> 33) as u8;
        }
    }
    let x: usize = unsafe { core::mem::transmute(buf) };
    return x;
}
