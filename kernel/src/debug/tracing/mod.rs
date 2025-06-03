mod events;
pub mod trace_pipe;

use crate::debug::sysfs::debugfs_kset;
use crate::driver::base::kobject::KObject;
use crate::filesystem::kernfs::callback::{KernCallbackData, KernFSCallback, KernInodePrivateData};
use crate::filesystem::kernfs::KernFSInode;
use crate::filesystem::vfs::syscall::ModeType;
use crate::filesystem::vfs::PollStatus;
use crate::libs::spinlock::SpinLock;
use crate::tracepoint::TracePointInfo;
use alloc::string::ToString;
use alloc::sync::Arc;
use system_error::SystemError;

static mut TRACING_ROOT_INODE: Option<Arc<KernFSInode>> = None;

static TRACE_RAW_PIPE: SpinLock<crate::tracepoint::TracePipeRaw> =
    SpinLock::new(crate::tracepoint::TracePipeRaw::new(4096));

static TRACE_CMDLINE_CACHE: SpinLock<crate::tracepoint::TraceCmdLineCache> =
    SpinLock::new(crate::tracepoint::TraceCmdLineCache::new(128));

pub fn trace_pipe_push_raw_record(record: &[u8]) {
    TRACE_RAW_PIPE.lock().push_event(record.to_vec());
}

pub fn trace_cmdline_push(pid: u32) {
    let process = crate::process::ProcessManager::current_pcb();
    let binding = process.basic();
    let pname = binding
        .name()
        .split(' ')
        .next()
        .unwrap_or("unknown")
        .split('/')
        .last()
        .unwrap_or("unknown");
    TRACE_CMDLINE_CACHE.lock().insert(pid, pname.to_string());
}

#[allow(unused)]
fn tracing_root_inode() -> Arc<KernFSInode> {
    unsafe { TRACING_ROOT_INODE.clone().unwrap() }
}

#[derive(Debug)]
pub struct TracingDirCallBack;

impl KernFSCallback for TracingDirCallBack {
    fn open(&self, _data: KernCallbackData) -> Result<(), SystemError> {
        Ok(())
    }

    fn read(
        &self,
        _data: KernCallbackData,
        _buf: &mut [u8],
        _offset: usize,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EISDIR)
    }

    fn write(
        &self,
        _data: KernCallbackData,
        _buf: &[u8],
        _offset: usize,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EISDIR)
    }

    fn poll(&self, _data: KernCallbackData) -> Result<PollStatus, SystemError> {
        Err(SystemError::EISDIR)
    }
}

impl KernInodePrivateData {
    pub fn debugfs_tracepoint(&self) -> Option<&Arc<TracePointInfo>> {
        return match self {
            KernInodePrivateData::DebugFS(tracepoint) => Some(tracepoint),
            _ => None,
        };
    }

    pub fn tracepipe(&mut self) -> Option<&mut crate::tracepoint::TracePipeSnapshot> {
        return match self {
            KernInodePrivateData::TracePipe(snapshot) => Some(snapshot),
            _ => None,
        };
    }
}

/// Initialize the debugfs tracing directory
pub fn init_debugfs_tracing() -> Result<(), SystemError> {
    let debugfs = debugfs_kset();
    let root_dir = debugfs.inode().ok_or(SystemError::ENOENT)?;
    let tracing_root = root_dir.add_dir(
        "tracing".to_string(),
        ModeType::from_bits_truncate(0o555),
        None,
        Some(&TracingDirCallBack),
    )?;
    let events_root = tracing_root.add_dir(
        "events".to_string(),
        ModeType::from_bits_truncate(0o755),
        None,
        Some(&TracingDirCallBack),
    )?;

    // tracing_root.add_file(
    //     "trace".to_string(),
    //     ModeType::from_bits_truncate(0o444),
    //     Some(4096),
    //     None,
    //     Some(&trace_pipe::TraceCallBack),
    // )?;

    tracing_root.add_file_lazy("trace".to_string(), trace_pipe::kernel_inode_provider)?;

    tracing_root.add_file(
        "trace_pipe".to_string(),
        ModeType::from_bits_truncate(0o444),
        Some(4096),
        None,
        Some(&trace_pipe::TracePipeCallBack),
    )?;

    events::init_events(events_root)?;

    unsafe {
        TRACING_ROOT_INODE = Some(tracing_root);
    }
    Ok(())
}
