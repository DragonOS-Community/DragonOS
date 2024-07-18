use core::ops::{Deref, DerefMut};

use crate::{KprobeBasic, KprobeBuilder, KprobeOps};

// #define BRK_KPROBE_BP		10	/* Kprobe break */
// #define BRK_KPROBE_SSTEPBP	11	/* Kprobe single step break */
const BRK_KPROBE_BP: u64 = 10;
const BRK_KPROBE_SSTEPBP: u64 = 11;
const EBREAK_INST: u32 = 0x002a0000;

#[derive(Debug)]
pub struct Kprobe {
    basic: KprobeBasic,
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

impl KprobeBuilder {
    pub fn build(self) -> Kprobe {
        Kprobe {
            basic: KprobeBasic::from(self),
            inst_tmp: [0; 8],
        }
    }
}

impl KprobeOps for Kprobe {
    fn install(self) -> Self {
        let address = self.symbol_addr + self.offset;
        let inst_tmp_ptr = self.inst_tmp.as_ptr() as usize;
        let inst_32 = unsafe { core::ptr::read(address as *const u32) };
        unsafe {
            core::ptr::write(address as *mut u32, EBREAK_INST);
            // inst_32 :0-32
            // ebreak  :32-64
            core::ptr::write(inst_tmp_ptr as *mut u32, inst_32);
            core::ptr::write((inst_tmp_ptr + 4) as *mut u32, EBREAK_INST);
        }
        unsafe {
            //
        }
        log::trace!(
            "Kprobe::install: address: {:#x}, func_name: {}, opcode: {:x?}",
            address,
            self.symbol,
            inst_32
        );
        self
    }

    fn return_address(&self) -> usize {
        let address = self.symbol_addr + self.offset;
        address + 4
    }

    fn single_step_address(&self) -> usize {
        self.inst_tmp.as_ptr() as usize
    }

    fn debug_address(&self) -> usize {
        self.inst_tmp.as_ptr() as usize + 4
    }
}

impl Drop for Kprobe {
    fn drop(&mut self) {
        let address = self.symbol_addr + self.offset;
        let inst_tmp_ptr = self.inst_tmp.as_ptr() as usize;
        let inst_32 = unsafe { core::ptr::read(inst_tmp_ptr as *const u32) };
        unsafe {
            core::ptr::write(address as *mut u32, inst_32);
        }
        log::trace!(
            "Kprobe::drop: address: {:#x}, func_name: {}, opcode: {:x?}",
            address,
            self.symbol,
            inst_32
        );
    }
}
