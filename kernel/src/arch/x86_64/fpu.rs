use core::{
    arch::{
        asm,
        x86_64::{_fxrstor64, _fxsave64}, global_asm,
    },
    ffi::c_void,
};

use alloc::boxed::Box;

use crate::include::bindings::bindings::pt_regs;

use crate::arch::asm::ptrace::user_mode;
use crate::current_pcb;
/// https://www.felixcloutier.com/x86/fxsave#tbl-3-47
/// 暂时性, 临时借鉴了rcore

#[repr(C, align(16))]
#[derive(Debug, Copy, Clone, Default)]
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

impl FpState {
    #[allow(dead_code)]
    pub fn new() -> Self {
        assert!(core::mem::size_of::<Self>() == 512);
        Self {
            mxcsr: 0x1f80,
            fcw: 0x037f,
            ..Self::default()
        }
    }
    #[allow(dead_code)]
    pub fn save(&mut self) {
        unsafe {
            _fxsave64(self as *mut FpState as *mut u8);
        }
    }
    #[allow(dead_code)]
    pub fn restore(&self) {
        unsafe {
            _fxrstor64(self as *const FpState as *const u8);
        }
    }
}



#[allow(dead_code)]
#[no_mangle]
pub extern "C" fn fp_state_save(regs: &pt_regs) {
    let fp = Box::leak(Box::new(FpState::default()));
    if user_mode(regs) {
        fp.save();
        current_pcb().fp_state = fp as *mut FpState as usize as *mut c_void;
        unsafe {
            asm!(
                "mov rax, cr4",
                "and ax,~(3<<9)",
                "mov cr4,rax",
                "mov rax, cr0",
                "and ax,~(02h)",
                "or ax, ~(0FFFBh)",
                "mov cr0,rax" /*
                              "mov rax, cr0",
                              "and ax, 0xFFFB",
                              "or ax,0x2",
                              "mov cr0,rax",
                              "mov rax, cr4",
                              "or ax,3<<9",
                              "mov cr4, rax" */
            )
        }
    }
}

#[allow(dead_code)]
#[no_mangle]
pub extern "C" fn fp_state_restore(regs: &pt_regs) {
    if user_mode(regs) {
        unsafe {
            asm! {
                "mov rax, cr0",
                "and ax, 0FFFBh",
                "or ax,2h",
                "mov cr0,rax",
                "mov rax, cr4",
                "or ax,3<<9",
                "mov cr4, rax"
            }
        }
        let fp = unsafe { Box::from_raw(current_pcb().fp_state as usize as *mut FpState) };
        fp.as_ref().restore();
    }
}
