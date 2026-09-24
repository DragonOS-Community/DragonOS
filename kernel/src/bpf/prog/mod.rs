pub mod device;
mod info;
mod tag;
mod util;
mod verifier;

use super::Result;
use crate::bpf::map::BpfMap;
use crate::bpf::prog::device::{DeviceAccess, DeviceProgram};
use crate::bpf::prog::util::{BpfProgMeta, BpfProgVerifierInfo};
use crate::bpf::prog::verifier::BpfProgVerifier;
use crate::filesystem::vfs::file::{File, FileFlags};
use crate::filesystem::vfs::InodeMode;
use crate::filesystem::vfs::{FilePrivateData, FileSystem, FileType, IndexNode, Metadata};
use crate::include::bindings::linux_bpf::{bpf_attr, bpf_prog_type, BPF_F_SLEEPABLE};
use crate::libs::mutex::MutexGuard;
use crate::libs::spinlock::SpinLock;
use crate::process::ProcessManager;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicU32, Ordering};
use hashbrown::HashMap;
use system_error::SystemError;

#[derive(Debug)]
pub struct BpfProg {
    id: u32,
    tag: [u8; 8],
    meta: BpfProgMeta,
    device: Option<DeviceProgram>,
    raw_file_ptr: Vec<usize>,
}

static NEXT_PROG_ID: AtomicU32 = AtomicU32::new(1);
lazy_static! {
    static ref PROGRAMS_BY_ID: SpinLock<HashMap<u32, Weak<BpfProg>>> =
        SpinLock::new(HashMap::new());
}

impl BpfProg {
    fn new(meta: BpfProgMeta, id: u32, device: Option<DeviceProgram>) -> Self {
        Self {
            id,
            tag: [0; 8],
            meta,
            device,
            raw_file_ptr: Vec::new(),
        }
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn tag(&self) -> [u8; 8] {
        self.tag
    }

    pub fn run_device(&self, access: DeviceAccess) -> bool {
        self.device
            .as_ref()
            .is_some_and(|program| program.run(access))
    }

    pub fn name(&self) -> &str {
        &self.meta.name
    }

    pub fn instruction_count(&self) -> u32 {
        (self.meta.insns.len() / 8) as u32
    }

    pub fn insns(&self) -> &[u8] {
        &self.meta.insns
    }

    pub fn insns_mut(&mut self) -> &mut [u8] {
        &mut self.meta.insns
    }

    pub fn prog_type(&self) -> bpf_prog_type {
        self.meta.prog_type
    }

    pub fn is_sleepable(&self) -> bool {
        self.meta.prog_flags & BPF_F_SLEEPABLE != 0
    }

    pub fn insert_map(&mut self, map_ptr: usize) {
        self.raw_file_ptr.push(map_ptr);
    }
}

impl IndexNode for BpfProg {
    fn open(&self, _data: MutexGuard<FilePrivateData>, _flags: &FileFlags) -> Result<()> {
        Ok(())
    }
    fn close(&self, _data: MutexGuard<FilePrivateData>) -> Result<()> {
        Ok(())
    }
    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize> {
        Err(SystemError::ENOSYS)
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize> {
        Err(SystemError::ENOSYS)
    }

    fn metadata(&self) -> Result<Metadata> {
        let meta = Metadata {
            mode: InodeMode::from_bits_truncate(0o755),
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

    fn absolute_path(&self) -> core::result::Result<String, SystemError> {
        Ok(String::from("BPF Program"))
    }
}

impl Drop for BpfProg {
    fn drop(&mut self) {
        PROGRAMS_BY_ID.lock().remove(&self.id);
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
    let log_info = BpfProgVerifierInfo::from(attr);
    let device = if args.prog_type == bpf_prog_type::BPF_PROG_TYPE_CGROUP_DEVICE {
        if args.prog_flags != 0 {
            return Err(SystemError::EINVAL);
        }
        Some(DeviceProgram::verify(&args.insns)?)
    } else {
        None
    };
    let id = NEXT_PROG_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .map_err(|_| SystemError::ENOSPC)?;
    let prog = BpfProg::new(args, id, device);
    let current = ProcessManager::current_pcb();
    let fd_table = current.fd_table();
    let mut prog = if prog.device.is_some() {
        prog
    } else {
        BpfProgVerifier::new(prog, log_info.log_level, &mut []).verify(&fd_table)?
    };
    prog.tag = tag::tag(prog.insns());
    let prog = Arc::new(prog);
    let file = File::new(prog.clone(), FileFlags::O_RDWR)?;
    let fd = fd_table
        .alloc_fd(file, false, current.nofile_soft_limit())
        .map(|x| x as usize)?;
    PROGRAMS_BY_ID.lock().insert(id, Arc::downgrade(&prog));
    Ok(fd)
}

pub fn program_by_id(id: u32) -> Option<Arc<BpfProg>> {
    PROGRAMS_BY_ID.lock().get(&id).and_then(Weak::upgrade)
}

pub(in crate::bpf) use info::{get_fd_by_id, get_info_by_fd};
