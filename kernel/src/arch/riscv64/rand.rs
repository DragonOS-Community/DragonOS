pub fn rand() -> usize {
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
