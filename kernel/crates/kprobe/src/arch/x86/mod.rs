use alloc::string::ToString;
use core::{
    fmt::Debug,
    ops::{Deref, DerefMut},
};

use yaxpeax_arch::LengthedInstruction;

use crate::{KprobeBasic, KprobeBuilder, KprobeOps};

const EBREAK_INST: u8 = 0xcc; // x86_64: 0xcc

pub struct Kprobe {
    basic: KprobeBasic,
    old_instruction: [u8; 15],
    old_instruction_len: usize,
}

impl Debug for Kprobe {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Kprobe")
            .field("basic", &self.basic)
            .field("old_instruction", &self.old_instruction)
            .field("old_instruction_len", &self.old_instruction_len)
            .finish()
    }
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

impl KprobeBuilder {
    pub fn build(self) -> Kprobe {
        Kprobe {
            basic: KprobeBasic::from(self),
            old_instruction: [0; 15],
            old_instruction_len: 0,
        }
    }
}

impl KprobeOps for Kprobe {
    fn install(mut self) -> Kprobe {
        let address = self.symbol_addr + self.offset;
        let max_instruction_size = 15; // x86_64 max instruction length
        let mut inst_tmp = [0u8; 15];
        unsafe {
            core::ptr::copy(
                address as *const u8,
                inst_tmp.as_mut_ptr(),
                max_instruction_size,
            );
        }

        let decoder = yaxpeax_x86::amd64::InstDecoder::default();

        let inst = decoder.decode_slice(&inst_tmp).unwrap();
        let len = inst.len().to_const();
        log::trace!("inst: {:?}, len: {:?}", inst.to_string(), len);

        self.old_instruction = inst_tmp;
        self.old_instruction_len = len as usize;
        unsafe {
            core::ptr::write_volatile(address as *mut u8, EBREAK_INST);
            core::arch::x86_64::_mm_mfence();
        }
        log::trace!(
            "Kprobe::install: address: {:#x}, func_name: {}",
            address,
            self.symbol
        );
        self
    }

    fn return_address(&self) -> usize {
        self.symbol_addr + self.offset + self.old_instruction_len
    }

    fn single_step_address(&self) -> usize {
        self.old_instruction.as_ptr() as usize
    }

    fn debug_address(&self) -> usize {
        self.old_instruction.as_ptr() as usize + self.old_instruction_len
    }
}

impl Drop for Kprobe {
    fn drop(&mut self) {
        let address = self.symbol_addr + self.offset;
        unsafe {
            core::ptr::copy(
                self.old_instruction.as_ptr(),
                address as *mut u8,
                self.old_instruction_len,
            );
            core::arch::x86_64::_mm_mfence();
        }
        let decoder = yaxpeax_x86::amd64::InstDecoder::default();
        let inst = decoder.decode_slice(&self.old_instruction).unwrap();
        log::trace!(
            "Kprobe::uninstall: address: {:#x}, old_instruction: {:?}",
            address,
            inst.to_string()
        );
    }
}
