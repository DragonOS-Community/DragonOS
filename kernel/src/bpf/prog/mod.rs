mod util;
mod verifier;

use super::Result;
use crate::bpf::map::BpfMap;
use crate::bpf::prog::util::{BpfProgMeta, BpfProgVerifierInfo};
use crate::bpf::prog::verifier::BpfProgVerifier;
use crate::filesystem::vfs::file::{File, FileMode};
use crate::filesystem::vfs::syscall::ModeType;
use crate::filesystem::vfs::{FilePrivateData, FileSystem, FileType, IndexNode, Metadata};
use crate::include::bindings::linux_bpf::bpf_attr;
use crate::libs::spinlock::SpinLockGuard;
use crate::process::ProcessManager;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use system_error::SystemError;

#[derive(Debug)]
pub struct BpfProg {
    meta: BpfProgMeta,
    raw_file_ptr: Vec<usize>,
}

impl BpfProg {
    pub fn new(meta: BpfProgMeta) -> Self {
        Self {
            meta,
            raw_file_ptr: Vec::new(),
        }
    }

    pub fn insns(&self) -> &[u8] {
        &self.meta.insns
    }

    pub fn insns_mut(&mut self) -> &mut [u8] {
        &mut self.meta.insns
    }

    pub fn insert_map(&mut self, map_ptr: usize) {
        self.raw_file_ptr.push(map_ptr);
    }
}

impl IndexNode for BpfProg {
    fn open(&self, _data: SpinLockGuard<FilePrivateData>, _mode: &FileMode) -> Result<()> {
        Ok(())
    }
    fn close(&self, _data: SpinLockGuard<FilePrivateData>) -> Result<()> {
        Ok(())
    }
    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize> {
        Err(SystemError::ENOSYS)
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize> {
        Err(SystemError::ENOSYS)
    }

    fn metadata(&self) -> Result<Metadata> {
        let meta = Metadata {
            mode: ModeType::from_bits_truncate(0o755),
            file_type: FileType::File,
            ..Default::default()
        };
        Ok(meta)
    }

    fn resize(&self, _len: usize) -> Result<()> {
        Ok(())
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        panic!("BpfProg does not have a filesystem")
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn list(&self) -> Result<Vec<String>> {
        Err(SystemError::ENOSYS)
    }
}

impl Drop for BpfProg {
    fn drop(&mut self) {
        unsafe {
            for ptr in self.raw_file_ptr.iter() {
                let file = Arc::from_raw(*ptr as *const u8 as *const BpfMap);
                drop(file)
            }
        }
    }
}
/// Load a BPF program into the kernel.
///
/// See https://ebpf-docs.dylanreimerink.nl/linux/syscall/BPF_PROG_LOAD/
pub fn bpf_prog_load(attr: &bpf_attr) -> Result<usize> {
    let args = BpfProgMeta::try_from(attr)?;
    // info!("bpf_prog_load: {:#?}", args);
    let log_info = BpfProgVerifierInfo::from(attr);
    let prog = BpfProg::new(args);
    let fd_table = ProcessManager::current_pcb().fd_table();
    let prog = BpfProgVerifier::new(prog, log_info.log_level, &mut []).verify(&fd_table)?;
    let file = File::new(Arc::new(prog), FileMode::O_RDWR)?;
    let fd = fd_table.write().alloc_fd(file, None).map(|x| x as usize)?;
    Ok(fd)
}
