use core::{
    arch::riscv64::sfence_vma_all,
    fmt::Debug,
    ops::{Deref, DerefMut},
};

use raki::{decode::Decode, Isa};

use crate::{KprobeBasic, KprobeBuilder, KprobeOps};
const EBREAK_INST: u32 = 0x00100073; // ebreak
const C_EBREAK_INST: u32 = 0x9002; // c.ebreak

#[derive(Debug)]
pub struct Kprobe {
    basic: KprobeBasic,
    old_instruction: OpcodeTy,
    inst_tmp: [u8; 8],
}

impl Deref for Kprobe {
    type Target = KprobeBasic;

    fn deref(&self) -> &Self::Target {
        &self.basic
    }
}

impl DerefMut for Kprobe {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.basic
    }
}

#[derive(Debug)]
enum OpcodeTy {
    Inst16(u16),
    Inst32(u32),
}

impl KprobeBuilder {
    pub fn build(self) -> Kprobe {
        Kprobe {
            basic: KprobeBasic::from(self),
            old_instruction: OpcodeTy::Inst32(0),
            inst_tmp: [0; 8],
        }
    }
}

impl KprobeOps for Kprobe {
    fn install(mut self) -> Self {
        let address = self.symbol_addr + self.offset;
        let inst_16 = unsafe { core::ptr::read(address as *const u16) };
        let is_inst_16 = inst_16.decode(Isa::Rv64).is_ok();

        let inst_tmp_ptr = self.inst_tmp.as_ptr() as usize;
        if is_inst_16 {
            self.old_instruction = OpcodeTy::Inst16(inst_16);
            unsafe {
                core::ptr::write(address as *mut u16, C_EBREAK_INST as u16);
                // inst_16 :0-16
                // c.ebreak:16-32
                core::ptr::write(inst_tmp_ptr as *mut u16, inst_16);
                core::ptr::write((inst_tmp_ptr + 2) as *mut u16, C_EBREAK_INST as u16);
            }
        } else {
            let inst_32 = unsafe { core::ptr::read(address as *const u32) };
            self.old_instruction = OpcodeTy::Inst32(inst_32);
            unsafe {
                core::ptr::write(address as *mut u32, EBREAK_INST);
                // inst_32 :0-32
                // ebreak  :32-64
                core::ptr::write(inst_tmp_ptr as *mut u32, inst_32);
                core::ptr::write((inst_tmp_ptr + 4) as *mut u32, EBREAK_INST);
            }
        }
        unsafe {
            sfence_vma_all();
        }
        log::trace!(
            "Kprobe::install: address: {:#x}, func_name: {}, opcode: {:x?}",
            address,
            self.symbol,
            self.old_instruction
        );
        self
    }

    fn return_address(&self) -> usize {
        let address = self.symbol_addr + self.offset;
        match self.old_instruction {
            OpcodeTy::Inst16(_) => address + 2,
            OpcodeTy::Inst32(_) => address + 4,
        }
    }

    fn single_step_address(&self) -> usize {
        self.inst_tmp.as_ptr() as usize
    }

    fn debug_address(&self) -> usize {
        match self.old_instruction {
            OpcodeTy::Inst16(_) => self.inst_tmp.as_ptr() as usize + 2,
            OpcodeTy::Inst32(_) => self.inst_tmp.as_ptr() as usize + 4,
        }
    }
}

impl Drop for Kprobe {
    fn drop(&mut self) {
        let address = self.symbol_addr + self.offset;
        match self.old_instruction {
            OpcodeTy::Inst16(inst_16) => unsafe {
                core::ptr::write(address as *mut u16, inst_16);
            },
            OpcodeTy::Inst32(inst_32) => unsafe {
                core::ptr::write(address as *mut u32, inst_32);
            },
        }
        unsafe {
            sfence_vma_all();
        }
        log::trace!(
            "Kprobe::uninstall: address: {:#x}, old_instruction: {:#x?}",
            address,
            self.old_instruction
        );
    }
}
