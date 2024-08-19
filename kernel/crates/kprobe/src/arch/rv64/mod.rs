use alloc::sync::Arc;
use core::{
    arch::riscv64::sfence_vma_all,
    fmt::Debug,
    ops::{Deref, DerefMut},
};

use crate::{KprobeBasic, KprobeBuilder, KprobeOps};
const EBREAK_INST: u32 = 0x00100073; // ebreak
const C_EBREAK_INST: u32 = 0x9002; // c.ebreak
const INSN_LENGTH_MASK: u16 = 0x3;
const INSN_LENGTH_32: u16 = 0x3;

#[derive(Debug)]
pub struct Kprobe {
    basic: KprobeBasic,
    point: Arc<Rv64KprobePoint>,
}

#[derive(Debug)]
enum OpcodeTy {
    Inst16(u16),
    Inst32(u32),
}
#[derive(Debug)]
pub struct Rv64KprobePoint {
    addr: usize,
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

impl Kprobe {
    pub fn probe_point(&self) -> &Arc<Rv64KprobePoint> {
        &self.point
    }
}

impl Drop for Rv64KprobePoint {
    fn drop(&mut self) {
        let address = self.addr;
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
            "Kprobe::uninstall: address: {:#x}, old_instruction: {:?}",
            address,
            self.old_instruction
        );
    }
}

impl KprobeBuilder {
    pub fn install(self) -> (Kprobe, Arc<Rv64KprobePoint>) {
        let probe_point = match &self.probe_point {
            Some(point) => point.clone(),
            None => self.replace_inst(),
        };
        let kprobe = Kprobe {
            basic: KprobeBasic::from(self),
            point: probe_point.clone(),
        };
        (kprobe, probe_point)
    }
    /// # 安装kprobe
    ///
    /// 不同的架构下需要保存原指令，然后替换为断点指令
    fn replace_inst(&self) -> Arc<Rv64KprobePoint> {
        let address = self.symbol_addr + self.offset;
        let inst_16 = unsafe { core::ptr::read(address as *const u16) };
        // See https://elixir.bootlin.com/linux/v6.10.2/source/arch/riscv/kernel/probes/kprobes.c#L68
        let is_inst_16 = if (inst_16 & INSN_LENGTH_MASK) == INSN_LENGTH_32 {
            false
        } else {
            true
        };
        let mut point = Rv64KprobePoint {
            old_instruction: OpcodeTy::Inst16(0),
            inst_tmp: [0; 8],
            addr: address,
        };
        let inst_tmp_ptr = point.inst_tmp.as_ptr() as usize;
        if is_inst_16 {
            point.old_instruction = OpcodeTy::Inst16(inst_16);
            unsafe {
                core::ptr::write(address as *mut u16, C_EBREAK_INST as u16);
                // inst_16 :0-16
                // c.ebreak:16-32
                core::ptr::write(inst_tmp_ptr as *mut u16, inst_16);
                core::ptr::write((inst_tmp_ptr + 2) as *mut u16, C_EBREAK_INST as u16);
            }
        } else {
            let inst_32 = unsafe { core::ptr::read(address as *const u32) };
            point.old_instruction = OpcodeTy::Inst32(inst_32);
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
            "Kprobe::install: address: {:#x}, func_name: {:?}, opcode: {:x?}",
            address,
            self.symbol,
            point.old_instruction
        );
        Arc::new(point)
    }
}

impl KprobeOps for Rv64KprobePoint {
    fn return_address(&self) -> usize {
        let address = self.addr;
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
    fn break_address(&self) -> usize {
        self.addr
    }
}
