#![allow(clippy::new_without_default)]

mod basic_macro;
mod point;
mod trace_pipe;

use alloc::{
    boxed::Box,
    collections::BTreeMap,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::{
    ops::{Deref, DerefMut},
    sync::atomic::AtomicUsize,
};
pub use point::{
    CommonTracePointMeta, TraceEntry, TracePoint, TracePointCallBackFunc, TracePointFunc,
};
use system_error::SystemError;
pub use trace_pipe::{
    TraceCmdLineCache, TraceEntryParser, TracePipeOps, TracePipeRaw, TracePipeSnapshot,
};

use crate::libs::spinlock::{SpinLock, SpinLockGuard};

#[derive(Debug)]
pub struct TracePointMap(BTreeMap<u32, &'static TracePoint>);

impl TracePointMap {
    /// Create a new TracePointMap
    fn new() -> Self {
        Self(BTreeMap::new())
    }
}

impl Deref for TracePointMap {
    type Target = BTreeMap<u32, &'static TracePoint>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for TracePointMap {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[derive(Debug)]
pub struct TracingEventsManager {
    subsystems: SpinLock<BTreeMap<String, Arc<EventsSubsystem>>>,
    map: SpinLock<TracePointMap>,
}

impl TracingEventsManager {
    fn new(map: TracePointMap) -> Self {
        Self {
            subsystems: SpinLock::new(BTreeMap::new()),
            map: SpinLock::new(map),
        }
    }

    /// Get the tracepoint map
    pub fn tracepoint_map(&self) -> SpinLockGuard<TracePointMap> {
        self.map.lock()
    }

    /// Create a subsystem by name
    ///
    /// If the subsystem already exists, return the existing subsystem.
    fn create_subsystem(&self, subsystem_name: &str) -> Arc<EventsSubsystem> {
        if self.subsystems.lock().contains_key(subsystem_name) {
            return self
                .get_subsystem(subsystem_name)
                .expect("Subsystem should exist");
        }
        let subsystem = Arc::new(EventsSubsystem::new());
        self.subsystems
            .lock()
            .insert(subsystem_name.to_string(), subsystem.clone());
        subsystem
    }

    /// Get the subsystem by name
    pub fn get_subsystem(&self, subsystem_name: &str) -> Option<Arc<EventsSubsystem>> {
        self.subsystems.lock().get(subsystem_name).cloned()
    }

    #[allow(unused)]
    /// Remove the subsystem by name
    pub fn remove_subsystem(&self, subsystem_name: &str) -> Option<Arc<EventsSubsystem>> {
        self.subsystems.lock().remove(subsystem_name)
    }

    /// Get all subsystems
    pub fn subsystem_names(&self) -> Vec<String> {
        let res = self
            .subsystems
            .lock()
            .keys()
            .cloned()
            .collect::<Vec<String>>();
        res
    }
}

#[derive(Debug)]
pub struct EventsSubsystem {
    events: SpinLock<BTreeMap<String, Arc<TracePointInfo>>>,
}

impl EventsSubsystem {
    fn new() -> Self {
        Self {
            events: SpinLock::new(BTreeMap::new()),
        }
    }

    /// Create an event by name
    fn create_event(&self, event_name: &str, event_info: TracePointInfo) {
        self.events
            .lock()
            .insert(event_name.to_string(), Arc::new(event_info));
    }

    /// Get the event by name
    pub fn get_event(&self, event_name: &str) -> Option<Arc<TracePointInfo>> {
        self.events.lock().get(event_name).cloned()
    }

    /// Get all events in the subsystem
    pub fn event_names(&self) -> Vec<String> {
        let res = self.events.lock().keys().cloned().collect::<Vec<String>>();
        res
    }
}
#[derive(Debug)]
pub struct TracePointInfo {
    enable: TracePointEnableFile,
    tracepoint: &'static TracePoint,
    format: TracePointFormatFile,
    id: TracePointIdFile,
    // filter:,
    // trigger:,
}

impl TracePointInfo {
    fn new(tracepoint: &'static TracePoint) -> Self {
        let enable = TracePointEnableFile::new(tracepoint);
        let format = TracePointFormatFile::new(tracepoint);
        let id = TracePointIdFile::new(tracepoint);
        Self {
            enable,
            tracepoint,
            format,
            id,
        }
    }

    /// Get the tracepoint
    pub fn tracepoint(&self) -> &'static TracePoint {
        self.tracepoint
    }

    /// Get the enable file
    pub fn enable_file(&self) -> &TracePointEnableFile {
        &self.enable
    }

    /// Get the format file
    pub fn format_file(&self) -> &TracePointFormatFile {
        &self.format
    }

    /// Get the ID file
    pub fn id_file(&self) -> &TracePointIdFile {
        &self.id
    }
}

/// TracePointFormatFile provides a way to get the format of the tracepoint.
#[derive(Debug, Clone)]
pub struct TracePointFormatFile {
    tracepoint: &'static TracePoint,
}

impl TracePointFormatFile {
    fn new(tracepoint: &'static TracePoint) -> Self {
        Self { tracepoint }
    }

    /// Read the tracepoint format
    ///
    /// Returns the format string of the tracepoint.
    pub fn read(&self) -> String {
        self.tracepoint.print_fmt()
    }
}

#[derive(Debug, Clone)]
pub struct TracePointEnableFile {
    tracepoint: &'static TracePoint,
}

impl TracePointEnableFile {
    fn new(tracepoint: &'static TracePoint) -> Self {
        Self { tracepoint }
    }

    /// Read the tracepoint status
    ///
    /// Returns true if the tracepoint is enabled, false otherwise.
    pub fn read(&self) -> &'static str {
        if self.tracepoint.is_enabled() {
            "1\n"
        } else {
            "0\n"
        }
    }
    /// Enable or disable the tracepoint
    pub fn write(&self, enable: char) {
        match enable {
            '1' => self.tracepoint.enable(),
            '0' => self.tracepoint.disable(),
            _ => {
                log::warn!("Invalid value for tracepoint enable: {}", enable);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct TracePointIdFile {
    tracepoint: &'static TracePoint,
}

impl TracePointIdFile {
    fn new(tracepoint: &'static TracePoint) -> Self {
        Self { tracepoint }
    }

    /// Read the tracepoint ID
    ///
    /// Returns the ID of the tracepoint.
    pub fn read(&self) -> String {
        format!("{}\n", self.tracepoint.id())
    }
}

extern "C" {
    fn _tracepoint();
    fn _etracepoint();
}

/// Initialize the tracing events
pub fn global_init_events() -> Result<TracingEventsManager, SystemError> {
    static TRACE_POINT_ID: AtomicUsize = AtomicUsize::new(0);
    let events_manager = TracingEventsManager::new(TracePointMap::new());
    let tracepoint_data_start = _tracepoint as usize as *mut CommonTracePointMeta;
    let tracepoint_data_end = _etracepoint as usize as *mut CommonTracePointMeta;
    log::info!(
        "tracepoint_data_start: {:#x}, tracepoint_data_end: {:#x}",
        tracepoint_data_start as usize,
        tracepoint_data_end as usize
    );
    let tracepoint_data_len = (tracepoint_data_end as usize - tracepoint_data_start as usize)
        / size_of::<CommonTracePointMeta>();
    let tracepoint_data =
        unsafe { core::slice::from_raw_parts_mut(tracepoint_data_start, tracepoint_data_len) };

    log::info!("tracepoint_data_len: {}", tracepoint_data_len);

    let mut tracepoint_map = events_manager.tracepoint_map();
    for tracepoint_meta in tracepoint_data {
        let tracepoint = tracepoint_meta.trace_point;
        let id = TRACE_POINT_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        tracepoint.set_id(id as u32);
        tracepoint.register(tracepoint_meta.print_func, Box::new(()));
        tracepoint_map.insert(id as u32, tracepoint);
        log::info!(
            "tracepoint registered: {}:{}",
            tracepoint.system(),
            tracepoint.name(),
        );
        let subsys_name = tracepoint.system();
        let subsys = events_manager.create_subsystem(subsys_name);
        let event_info = TracePointInfo::new(tracepoint);
        subsys.create_event(tracepoint.name(), event_info);
    }
    drop(tracepoint_map); // Release the lock on the tracepoint map
    Ok(events_manager)
}
