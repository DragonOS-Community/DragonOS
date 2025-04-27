use crate::debug::tracing::tracepoint::{CommonTracePointMeta, TracePoint};
use crate::debug::tracing::TracingDirCallBack;
use crate::filesystem::kernfs::callback::{KernCallbackData, KernFSCallback, KernInodePrivateData};
use crate::filesystem::kernfs::KernFSInode;
use crate::filesystem::vfs::syscall::ModeType;
use crate::filesystem::vfs::PollStatus;
use crate::libs::spinlock::SpinLock;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use system_error::SystemError;

#[derive(Debug)]
pub struct TracingEventsManager {
    root: Arc<KernFSInode>,
    subsystems: SpinLock<BTreeMap<String, Arc<EventsSubsystem>>>,
}

impl TracingEventsManager {
    pub fn new(root: Arc<KernFSInode>) -> Self {
        Self {
            root,
            subsystems: SpinLock::new(BTreeMap::new()),
        }
    }

    /// Create a subsystem by name
    ///
    /// If the subsystem already exists, return the existing subsystem.
    pub fn create_subsystem(
        &self,
        subsystem_name: &str,
    ) -> Result<Arc<EventsSubsystem>, SystemError> {
        if self.subsystems.lock().contains_key(subsystem_name) {
            return Ok(self.get_subsystem(subsystem_name).unwrap());
        }
        let dir = self.root.add_dir(
            subsystem_name.to_string(),
            ModeType::from_bits_truncate(0o755),
            None,
            Some(&TracingDirCallBack),
        )?;
        let subsystem = Arc::new(EventsSubsystem::new(dir));
        self.subsystems
            .lock()
            .insert(subsystem_name.to_string(), subsystem.clone());
        Ok(subsystem)
    }

    /// Get the subsystem by name
    pub fn get_subsystem(&self, subsystem_name: &str) -> Option<Arc<EventsSubsystem>> {
        self.subsystems.lock().get(subsystem_name).cloned()
    }
}

#[derive(Debug)]
pub struct EventsSubsystem {
    root: Arc<KernFSInode>,
    events: SpinLock<BTreeMap<String, Arc<EventInfo>>>,
}

impl EventsSubsystem {
    pub fn new(root: Arc<KernFSInode>) -> Self {
        Self {
            root,
            events: SpinLock::new(BTreeMap::new()),
        }
    }

    /// Insert a new event into the subsystem
    pub fn insert_event(
        &self,
        event_name: &str,
        event_info: Arc<EventInfo>,
    ) -> Result<(), SystemError> {
        self.events
            .lock()
            .insert(event_name.to_string(), event_info);
        Ok(())
    }

    /// Get the event by name
    #[allow(unused)]
    pub fn get_event(&self, event_name: &str) -> Option<Arc<EventInfo>> {
        self.events.lock().get(event_name).cloned()
    }

    /// Get the root inode of the subsystem
    pub fn root(&self) -> Arc<KernFSInode> {
        self.root.clone()
    }
}

#[derive(Debug)]
pub struct EventInfo {
    #[allow(unused)]
    enable: Arc<KernFSInode>,
    // filter: Arc<KernFSInode>,
    // trigger: Arc<KernFSInode>,
}

impl EventInfo {
    pub fn new(tracepoint: &'static TracePoint, subsystem: Arc<KernFSInode>) -> Arc<Self> {
        let trace_dir = subsystem
            .add_dir(
                tracepoint.name().to_string(),
                ModeType::from_bits_truncate(0o755),
                None,
                Some(&TracingDirCallBack),
            )
            .expect("add tracepoint dir failed");
        let enable_inode = trace_dir
            .add_file(
                "enable".to_string(),
                ModeType::from_bits_truncate(0o644),
                None,
                Some(KernInodePrivateData::DebugFS(tracepoint)),
                Some(&EnableCallBack),
            )
            .expect("add enable file failed");

        Arc::new(Self {
            enable: enable_inode,
        })
    }
}

impl Drop for EventInfo {
    fn drop(&mut self) {}
}

#[derive(Debug)]
struct EnableCallBack;

impl KernFSCallback for EnableCallBack {
    fn open(&self, _data: KernCallbackData) -> Result<(), SystemError> {
        Ok(())
    }

    fn read(
        &self,
        data: KernCallbackData,
        buf: &mut [u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        if offset > 0 {
            return Ok(0);
        }
        let pri_data = data.private_data();
        match pri_data {
            Some(pri_data) => {
                let tracepoint = pri_data.debugfs_tracepoint().ok_or(SystemError::EINVAL)?;
                let buf_value = if tracepoint.is_enabled() { b"1" } else { b"0" };
                let len = buf.len().min(buf_value.len());
                buf[..len].copy_from_slice(&buf_value[..len]);
                Ok(len)
            }
            None => {
                return Err(SystemError::EINVAL);
            }
        }
    }

    fn write(
        &self,
        data: KernCallbackData,
        buf: &[u8],
        _offset: usize,
    ) -> Result<usize, SystemError> {
        let pri_data = data.private_data();
        match pri_data {
            Some(pri_data) => {
                let tracepoint = pri_data.debugfs_tracepoint().ok_or(SystemError::EINVAL)?;
                let value = core::str::from_utf8(buf)
                    .map_err(|_| SystemError::EINVAL)?
                    .trim();
                match value {
                    "0" => {
                        tracepoint.disable();
                    }
                    "1" => {
                        tracepoint.enable();
                    }
                    _ => {
                        log::info!("EnableCallBack invalid value: {}", value);
                        return Err(SystemError::EINVAL);
                    }
                }
                Ok(buf.len())
            }
            None => {
                return Err(SystemError::EINVAL);
            }
        }
    }

    fn poll(&self, _data: KernCallbackData) -> Result<PollStatus, SystemError> {
        Err(SystemError::ENOSYS)
    }
}

static mut TRACING_EVENTS_MANAGER: Option<TracingEventsManager> = None;

/// Initialize the tracing events
pub fn init_events(events_root: Arc<KernFSInode>) -> Result<(), SystemError> {
    let events_manager = TracingEventsManager::new(events_root);
    let tracepoint_data_start = _tracepoint as usize as *const CommonTracePointMeta;
    let tracepoint_data_end = _etracepoint as usize as *const CommonTracePointMeta;
    let tracepoint_data_len = (tracepoint_data_end as usize - tracepoint_data_start as usize)
        / size_of::<CommonTracePointMeta>();
    let tracepoint_data =
        unsafe { core::slice::from_raw_parts(tracepoint_data_start, tracepoint_data_len) };
    for tracepoint_meta in tracepoint_data {
        let tracepoint = tracepoint_meta.trace_point;
        tracepoint.register(tracepoint_meta.print_func, Box::new(()));
        log::info!(
            "tracepoint name: {}, module path: {}",
            tracepoint.name(),
            tracepoint.module_path()
        );
        // kernel::{subsystem}::
        let mut subsys_name = tracepoint.module_path().split("::");
        let subsys_name = subsys_name.nth(1).ok_or(SystemError::EINVAL)?;
        let subsys = events_manager.create_subsystem(subsys_name)?;
        let event_info = EventInfo::new(tracepoint, subsys.root());
        subsys.insert_event(tracepoint.name(), event_info)?;
    }

    unsafe {
        TRACING_EVENTS_MANAGER = Some(events_manager);
    }

    Ok(())
}
extern "C" {
    fn _tracepoint();
    fn _etracepoint();
}
