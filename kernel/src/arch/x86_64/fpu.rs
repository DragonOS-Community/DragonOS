use core::arch::x86_64::{_fxrstor64, _fxsave64};

/// https://www.felixcloutier.com/x86/fxsave#tbl-3-47
#[repr(C, align(16))]
#[derive(Debug, Copy, Clone)]
pub struct FpState {
    //0
    fcw: u16,
    fsw: u16,
    ftw: u16,
    fop: u16,
    word2: u64,
    //16
    word3: u64,
    mxcsr: u32,
    mxcsr_mask: u32,
    //32
    mm: [u64; 16],
    //160
    xmm: [u64; 32],
    //416
    rest: [u64; 12],
}

impl Default for FpState {
    fn default() -> Self {
        Self {
            fcw: 0x037f,
            fsw: Default::default(),
            ftw: Default::default(),
            fop: Default::default(),
            word2: Default::default(),
            word3: Default::default(),
            mxcsr: 0x1f80,
            mxcsr_mask: Default::default(),
            mm: Default::default(),
            xmm: Default::default(),
            rest: Default::default(),
        }
    }
}
impl FpState {
    #[inline]
    pub fn new() -> Self {
        assert!(core::mem::size_of::<Self>() == 512);
        return Self::default();
    }

    #[inline]
    pub fn save(&mut self) {
        unsafe {
            _fxsave64(self as *mut FpState as *mut u8);
        }
    }

    #[inline]
    pub fn restore(&self) {
        unsafe {
            _fxrstor64(self as *const FpState as *const u8);
        }
    }

    /// 清空浮点寄存器
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        *self = Self::default();
        self.restore();
    }
}
