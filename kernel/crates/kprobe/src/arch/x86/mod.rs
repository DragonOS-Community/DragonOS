use crate::{KprobeBasic, KprobeBuilder, KprobeOps};
use alloc::string::ToString;
use alloc::sync::Arc;
use core::{
    fmt::Debug,
    ops::{Deref, DerefMut},
};
use yaxpeax_arch::LengthedInstruction;

const EBREAK_INST: u8 = 0xcc; // x86_64: 0xcc
const MAX_INSTRUCTION_SIZE: usize = 15; // x86_64 max instruction length

pub struct Kprobe {
    basic: KprobeBasic,
    point: Arc<X86KprobePoint>,
}

#[derive(Debug)]
pub struct X86KprobePoint {
    addr: usize,
    old_instruction: [u8; MAX_INSTRUCTION_SIZE],
    old_instruction_len: usize,
}

impl Drop for X86KprobePoint {
    fn drop(&mut self) {
        let address = self.addr;
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

impl Debug for Kprobe {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Kprobe")
            .field("basic", &self.basic)
            .field("point", &self.point)
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
    pub fn install(self) -> (Kprobe, Arc<X86KprobePoint>) {
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
    fn replace_inst(&self) -> Arc<X86KprobePoint> {
        let address = self.symbol_addr + self.offset;
        let mut inst_tmp = [0u8; MAX_INSTRUCTION_SIZE];
        unsafe {
            core::ptr::copy(
                address as *const u8,
                inst_tmp.as_mut_ptr(),
                MAX_INSTRUCTION_SIZE,
            );
        }
        let decoder = yaxpeax_x86::amd64::InstDecoder::default();
        let inst = decoder.decode_slice(&inst_tmp).unwrap();
        let len = inst.len().to_const();
        log::trace!("inst: {:?}, len: {:?}", inst.to_string(), len);
        let point = Arc::new(X86KprobePoint {
            addr: address,
            old_instruction: inst_tmp,
            old_instruction_len: len as usize,
        });
        unsafe {
            core::ptr::write_volatile(address as *mut u8, EBREAK_INST);
            core::arch::x86_64::_mm_mfence();
        }
        log::trace!(
            "Kprobe::install: address: {:#x}, func_name: {:?}",
            address,
            self.symbol
        );
        point
    }
}

impl Kprobe {
    pub fn probe_point(&self) -> &Arc<X86KprobePoint> {
        &self.point
    }
}

impl KprobeOps for X86KprobePoint {
    fn return_address(&self) -> usize {
        self.addr + self.old_instruction_len
    }
    fn single_step_address(&self) -> usize {
        self.old_instruction.as_ptr() as usize
    }
    fn debug_address(&self) -> usize {
        self.old_instruction.as_ptr() as usize + self.old_instruction_len
    }
    fn break_address(&self) -> usize {
        self.addr
    }
}
