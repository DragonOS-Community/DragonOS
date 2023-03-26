use core::{
    arch::{
        asm, global_asm,
        x86_64::{_fxrstor64, _fxsave64},
    },
    ffi::c_void,
};

use alloc::boxed::Box;

use crate::{include::bindings::bindings::pt_regs, kdebug, println};

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

    #[allow(dead_code)]
    pub fn dup_fpstate(&self) -> Self {
        // Self {
        //     //0
        //     fcw: self.fcw,
        //     fsw: self.fsw,
        //     ftw: self.ftw,
        //     fop: self.fop,
        //     word2: self.word2,
        //     //16
        //     word3: self.word3,
        //     mxcsr: self.mxcsr,
        //     mxcsr_mask: self.mxcsr_mask,
        //     //32
        //     mm: self.mm.clone(),
        //     //160
        //     xmm: self.xmm.clone(),
        //     //416
        //     rest: self.rest.clone(),
        // }
        return self.clone();
    }
}

#[allow(dead_code)]
#[no_mangle]
pub extern "C" fn fp_state_save(regs: &pt_regs) {
    if user_mode(regs) {
        let fp = Box::leak(Box::new(FpState::default()));
        let i:u64;
        unsafe{
            asm!(
                "mov {0},cr4",
                out(reg) i,
            );
        }
        let j:u64;
        unsafe{
            asm!(
                "mov {0},cr0 ",
                out(reg) j,
            )
        }
        kdebug!("before fp_state_save   : cr0: {:#032b}, cr4: {:#032b}, pid={}",
        j,i,current_pcb().pid);
        fp.save();
        current_pcb().fp_state = fp as *mut FpState as usize as *mut c_void;
        unsafe {
            asm!(
                "mov rax, cr4",
                "and ax,~(3<<9)",//[9][10]->0
                "mov cr4,rax",
                "mov rax, cr0",
                "and ax,~(02h)",//[1]->0
                "or ax, ~(0FFFBh)",//[2]->1
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
    //("HELLO_WORLD");

    //kdebug!("hello world");
    if current_pcb().pid ==4{
        kdebug!("the user mode of pid 4:{}",user_mode(regs));
    }
    if user_mode(regs) {
        let i:u64;
        unsafe{
            asm!(
                "mov {0},cr4",
                out(reg) i,
            );
        }
        let j:u64;
        unsafe{
            asm!(
                "mov {0},cr0 ",
                out(reg) j,
            )
        }
        kdebug!("before fp_state_restore: cr0: {:#032b}, cr4: {:#032b}, pid={}",
        j,i,current_pcb().pid);
        //kdebug!("before fp_state_restore: ",current_pcb().pid);
        unsafe {
            asm! {
                "mov rax, cr0",
                "and ax, 0FFFBh",//[2]->0
                "or ax,02h",//[1]->1
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
