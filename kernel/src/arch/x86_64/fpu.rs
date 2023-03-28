use core::{
    arch::{
        asm,
        x86_64::{_fxrstor64, _fxsave64},
    },
    ffi::c_void,
    ptr::null_mut,
};

use alloc::boxed::Box;

use crate::include::bindings::bindings::process_control_block;

use super::asm::irqflags::{local_irq_restore, local_irq_save};
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
    #[allow(dead_code)]
    pub fn new() -> Self {
        assert!(core::mem::size_of::<Self>() == 512);
        return Self::default();
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

    /// @brief 清空fp_state
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

/// @brief 从用户态进入内核时，保存浮点寄存器，并关闭浮点功能
pub fn fp_state_save(pcb: &mut process_control_block) {
    // 该过程中不允许中断
    let mut rflags: u64 = 0;
    local_irq_save(&mut rflags);

    let fp: &mut FpState = if pcb.fp_state == null_mut() {
        let f = Box::leak(Box::new(FpState::default()));
        pcb.fp_state = f as *mut FpState as usize as *mut c_void;
        f
    } else {
        unsafe { (pcb.fp_state as usize as *mut FpState).as_mut().unwrap() }
    };

    // 保存浮点寄存器
    fp.save();

    // 关闭浮点功能
    unsafe {
        asm!(
            "mov rax, cr4",
            "and ax,~(3<<9)", //[9][10]->0
            "mov cr4,rax",
            "mov rax, cr0",
            "and ax,~(02h)",    //[1]->0
            "or ax, ~(0FFFBh)", //[2]->1
            "mov cr0, rax"      /*
                                "mov rax, cr0",
                                "and ax, 0xFFFB",
                                "or ax,0x2",
                                "mov cr0,rax",
                                "mov rax, cr4",
                                "or ax,3<<9",
                                "mov cr4, rax" */
        )
    }
    local_irq_restore(&rflags);
}

/// @brief 从内核态返回用户态时，恢复浮点寄存器，并开启浮点功能
pub fn fp_state_restore(pcb: &mut process_control_block) {
    // 该过程中不允许中断
    let mut rflags: u64 = 0;
    local_irq_save(&mut rflags);

    if pcb.fp_state == null_mut() {
        panic!("fp_state_restore: fp_state is null. pid={}", pcb.pid);
    }

    unsafe {
        asm! {
            "mov rax, cr0",
            "and ax, 0FFFBh",//[2]->0
            "or ax,02h",//[1]->1
            "mov cr0,rax",
            "mov rax, cr4",
            "or ax,3<<9",
            "mov cr4, rax",
            "clts",
            "fninit"
        }
    }

    let fp = unsafe { (pcb.fp_state as usize as *mut FpState).as_mut().unwrap() };
    fp.restore();
    fp.clear();

    local_irq_restore(&rflags);
}
