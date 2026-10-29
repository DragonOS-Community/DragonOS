use super::super::Result;
use crate::bpf::map::map_by_fd;
use crate::bpf::prog::instructions::{self, LdDwImm};
use crate::bpf::prog::util::VerifierLogLevel;
use crate::bpf::prog::BpfProg;
use crate::filesystem::vfs::fdtable::FileDescriptorTable;
use alloc::sync::Arc;
use system_error::SystemError;

/// Linux rejects a direct value offset that cannot be encoded in the immediate
/// pair (`BPF_MAX_VAR_OFF`).
const BPF_MAX_VAR_OFF: u32 = 1 << 29;

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

    /// Resolve the immediate of every `LD_DW_IMM` into the address the
    /// interpreter uses, mirroring Linux `resolve_pseudo_ldimm64()`.
    ///
    /// A single pass both validates the instruction layout and relocates map
    /// references: the slot width is only known here, and every map the
    /// program points at is kept alive by `hold_map` for as long as the
    /// program exists.
    fn relocation(&mut self, fd_table: &Arc<FileDescriptorTable>) -> Result<()> {
        let count = self.prog.insns().len() / instructions::INSN_SIZE;
        let mut index = 0;
        while index < count {
            if !instructions::check_slot(self.prog.insns(), index)? {
                index += 1;
                continue;
            }

            match instructions::decode_ld_dw_imm(self.prog.insns(), index)? {
                LdDwImm::Imm64 => {}
                LdDwImm::MapFd { fd } => {
                    let map = map_by_fd(fd_table, fd as i32)?;
                    let addr = Arc::as_ptr(&map) as u64;
                    self.prog.hold_map(map)?;
                    instructions::write_imm64(self.prog.insns_mut(), index, addr);
                }
                LdDwImm::MapValue { fd, offset } => {
                    // Linux resolves the descriptor before it bounds the
                    // direct value offset, so a bad fd wins over a bad offset.
                    let map = map_by_fd(fd_table, fd as i32)?;
                    if offset >= BPF_MAX_VAR_OFF {
                        return Err(SystemError::EINVAL);
                    }
                    let addr = map.direct_value_ptr(offset)? as u64;
                    self.prog.hold_map(map)?;
                    instructions::write_imm64(self.prog.insns_mut(), index, addr);
                }
            }
            index += 2;
        }
        Ok(())
    }

    pub fn verify(mut self, fd_table: &Arc<FileDescriptorTable>) -> Result<BpfProg> {
        self.relocation(fd_table)?;
        Ok(self.prog)
    }
}
