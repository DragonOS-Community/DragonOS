use super::super::Result;
use crate::bpf::map::BpfMap;
use crate::bpf::prog::util::VerifierLogLevel;
use crate::bpf::prog::BpfProg;
use crate::filesystem::vfs::file::FileDescriptorVec;
use crate::include::bindings::linux_bpf::*;
use crate::libs::casting::DowncastArc;
use crate::libs::rwlock::RwLock;
use alloc::{sync::Arc, vec::Vec};
use log::{error, info};
use rbpf::ebpf;
use rbpf::ebpf::to_insn_vec;
use system_error::SystemError;

/// The BPF program verifier.
///
/// See https://docs.kernel.org/bpf/verifier.html
#[derive(Debug)]
pub struct BpfProgVerifier<'a> {
    prog: BpfProg,
    _log_level: VerifierLogLevel,
    _log_buf: &'a mut [u8],
}

impl<'a> BpfProgVerifier<'a> {
    pub fn new(prog: BpfProg, log_level: VerifierLogLevel, log_buf: &'a mut [u8]) -> Self {
        Self {
            prog,
            _log_level: log_level,
            _log_buf: log_buf,
        }
    }
    /// Relocate the program.
    ///
    /// This function will relocate the program, and update the program's instructions.
    fn relocation(&mut self, fd_table: &Arc<RwLock<FileDescriptorVec>>) -> Result<()> {
        let instructions = self.prog.insns_mut();
        let mut fmt_insn = to_insn_vec(instructions);
        let mut index = 0;
        let mut raw_file_ptr = vec![];
        loop {
            if index >= fmt_insn.len() {
                break;
            }
            let mut insn = fmt_insn[index].clone();
            if insn.opc == ebpf::LD_DW_IMM {
                // relocate the instruction
                let mut next_insn = fmt_insn[index + 1].clone();
                // the imm is the map_fd because user lib has already done the relocation
                let map_fd = insn.imm as usize;
                let src_reg = insn.src;
                // See https://www.kernel.org/doc/html/latest/bpf/standardization/instruction-set.html#id23
                let ptr = match src_reg as u32 {
                    BPF_PSEUDO_MAP_VALUE => {
                        // dst = map_val(map_by_fd(imm)) + next_imm
                        // map_val(map) gets the address of the first value in a given map
                        let file = fd_table
                            .read()
                            .get_file_by_fd(map_fd as i32)
                            .ok_or(SystemError::EBADF)?;
                        let bpf_map = file
                            .inode()
                            .downcast_arc::<BpfMap>()
                            .ok_or(SystemError::EINVAL)?;
                        let first_value_ptr =
                            bpf_map.inner_map().lock().first_value_ptr()? as usize;
                        let offset = next_insn.imm as usize;
                        info!(
                            "Relocate for BPF_PSEUDO_MAP_VALUE, instruction index: {}, map_fd: {}",
                            index, map_fd
                        );
                        Some(first_value_ptr + offset)
                    }
                    BPF_PSEUDO_MAP_FD => {
                        // dst = map_by_fd(imm)
                        // map_by_fd(imm) means to convert a 32-bit file descriptor into an address of a map
                        let bpf_map = fd_table
                            .read()
                            .get_file_by_fd(map_fd as i32)
                            .ok_or(SystemError::EBADF)?
                            .inode()
                            .downcast_arc::<BpfMap>()
                            .ok_or(SystemError::EINVAL)?;
                        // todo!(warning: We need release after prog unload)
                        let map_ptr = Arc::into_raw(bpf_map) as usize;
                        info!(
                            "Relocate for BPF_PSEUDO_MAP_FD, instruction index: {}, map_fd: {}, ptr: {:#x}",
                            index, map_fd, map_ptr
                        );
                        raw_file_ptr.push(map_ptr);
                        Some(map_ptr)
                    }
                    ty => {
                        error!(
                            "relocation for ty: {} not implemented, instruction index: {}",
                            ty, index
                        );
                        None
                    }
                };
                if let Some(ptr) = ptr {
                    // The current ins store the map_data_ptr low 32 bits,
                    // the next ins store the map_data_ptr high 32 bits
                    insn.imm = ptr as i32;
                    next_insn.imm = (ptr >> 32) as i32;
                    fmt_insn[index] = insn;
                    fmt_insn[index + 1] = next_insn;
                    index += 2;
                } else {
                    index += 1;
                }
            } else {
                index += 1;
            }
        }
        let fmt_insn = fmt_insn
            .iter()
            .flat_map(|ins| ins.to_vec())
            .collect::<Vec<u8>>();
        instructions.copy_from_slice(&fmt_insn);
        for ptr in raw_file_ptr {
            self.prog.insert_map(ptr);
        }
        Ok(())
    }

    pub fn verify(mut self, fd_table: &Arc<RwLock<FileDescriptorVec>>) -> Result<BpfProg> {
        self.relocation(fd_table)?;
        Ok(self.prog)
    }
}
