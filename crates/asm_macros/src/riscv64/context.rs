/// 保存x6-x31寄存器
#[macro_export]
macro_rules! save_from_x6_to_x31 {
    () => {
        concat!(
            "
            sd x6, {off_t1}(sp)
            sd x7, {off_t2}(sp)
            sd x8, {off_s0}(sp)
            sd x9, {off_s1}(sp)
            sd x10, {off_a0}(sp)
            sd x11, {off_a1}(sp)
            sd x12, {off_a2}(sp)
            sd x13, {off_a3}(sp)
            sd x14, {off_a4}(sp)
            sd x15, {off_a5}(sp)
            sd x16, {off_a6}(sp)
            sd x17, {off_a7}(sp)
            sd x18, {off_s2}(sp)
            sd x19, {off_s3}(sp)
            sd x20, {off_s4}(sp)
            sd x21, {off_s5}(sp)
            sd x22, {off_s6}(sp)
            sd x23, {off_s7}(sp)
            sd x24, {off_s8}(sp)
            sd x25, {off_s9}(sp)
            sd x26, {off_s10}(sp)
            sd x27, {off_s11}(sp)
            sd x28, {off_t3}(sp)
            sd x29, {off_t4}(sp)
            sd x30, {off_t5}(sp)
            sd x31, {off_t6}(sp)

        "
        )
    };
}

#[macro_export]
macro_rules! restore_from_x6_to_x31 {
    () => {
        concat!("
        
            ld x6, {off_t1}(sp)
            ld x7, {off_t2}(sp)
            ld x8, {off_s0}(sp)
            ld x9, {off_s1}(sp)
            ld x10, {off_a0}(sp)
            ld x11, {off_a1}(sp)
            ld x12, {off_a2}(sp)
            ld x13, {off_a3}(sp)
            ld x14, {off_a4}(sp)
            ld x15, {off_a5}(sp)
            ld x16, {off_a6}(sp)
            ld x17, {off_a7}(sp)
            ld x18, {off_s2}(sp)
            ld x19, {off_s3}(sp)
            ld x20, {off_s4}(sp)
            ld x21, {off_s5}(sp)
            ld x22, {off_s6}(sp)
            ld x23, {off_s7}(sp)
            ld x24, {off_s8}(sp)
            ld x25, {off_s9}(sp)
            ld x26, {off_s10}(sp)
            ld x27, {off_s11}(sp)
            ld x28, {off_t3}(sp)
            ld x29, {off_t4}(sp)
            ld x30, {off_t5}(sp)
            ld x31, {off_t6}(sp)
        ")
    };
}
