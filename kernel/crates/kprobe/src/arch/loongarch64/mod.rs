use alloc::sync::Arc;
use core::ops::{Deref, DerefMut};

use crate::{KprobeBasic, KprobeBuilder, KprobeOps};

const BRK_KPROBE_BP: u64 = 10;
const BRK_KPROBE_SSTEPBP: u64 = 11;
const EBREAK_INST: u32 = 0x002a0000;

#[derive(Debug)]
pub struct Kprobe {
    basic: KprobeBasic,
    point: Arc<LA64KprobePoint>,
}
#[derive(Debug)]
pub struct LA64KprobePoint {
    addr: usize,
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
    pub fn probe_point(&self) -> &Arc<LA64KprobePoint> {
        &self.point
    }
}

impl Drop for LA64KprobePoint {
    fn drop(&mut self) {
        let address = self.addr;
        let inst_tmp_ptr = self.inst_tmp.as_ptr() as usize;
        let inst_32 = unsafe { core::ptr::read(inst_tmp_ptr as *const u32) };
        unsafe {
            core::ptr::write(address as *mut u32, inst_32);
        }
        log::trace!(
            "Kprobe::uninstall: address: {:#x}, old_instruction: {:?}",
            address,
            inst_32
        );
    }
}

impl KprobeBuilder {
    pub fn install(self) -> (Kprobe, Arc<LA64KprobePoint>) {
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
    fn replace_inst(&self) -> Arc<LA64KprobePoint> {
        let address = self.symbol_addr + self.offset;
        let point = LA64KprobePoint {
            addr: address,
            inst_tmp: [0u8; 8],
        };
        let inst_tmp_ptr = point.inst_tmp.as_ptr() as usize;
        let inst_32 = unsafe { core::ptr::read(address as *const u32) };
        unsafe {
            core::ptr::write(address as *mut u32, EBREAK_INST);
            // inst_32 :0-32
            // ebreak  :32-64
            core::ptr::write(inst_tmp_ptr as *mut u32, inst_32);
            core::ptr::write((inst_tmp_ptr + 4) as *mut u32, EBREAK_INST);
        }
        log::trace!(
            "Kprobe::install: address: {:#x}, func_name: {:?}, opcode: {:x?}",
            address,
            self.symbol,
            inst_32
        );
    }
}

impl KprobeOps for LA64KprobePoint {
    fn return_address(&self) -> usize {
        self.addr + 4
    }

    fn single_step_address(&self) -> usize {
        self.inst_tmp.as_ptr() as usize
    }

    fn debug_address(&self) -> usize {
        self.inst_tmp.as_ptr() as usize + 4
    }

    fn break_address(&self) -> usize {
        self.addr
    }
}
