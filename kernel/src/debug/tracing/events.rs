use alloc::string::ToString;

use crate::debug::tracing::TracingDirCallBack;
use crate::filesystem::kernfs::KernFSInode;
use crate::filesystem::kernfs::callback::{KernCallbackData, KernFSCallback, KernInodePrivateData};
use crate::filesystem::vfs::PollStatus;
use crate::filesystem::vfs::syscall::ModeType;
use crate::tracepoint::*;
use alloc::sync::Arc;
use system_error::SystemError;

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
        let pri_data = data.private_data().as_ref().unwrap();
        let tracepoint_info = pri_data.debugfs_tracepoint().unwrap();
        let enable_value = tracepoint_info.enable_file().read();
        if offset >= enable_value.as_bytes().len() {
            return Ok(0); // Offset is beyond the length of the string
        }
        let len = buf.len().min(enable_value.as_bytes().len() - offset);
        buf[..len].copy_from_slice(&enable_value.as_bytes()[offset..offset + len]);
        Ok(len)
    }

    fn write(
        &self,
        data: KernCallbackData,
        buf: &[u8],
        _offset: usize,
    ) -> Result<usize, SystemError> {
        let pri_data = data.private_data().as_ref().unwrap();
        let tracepoint = pri_data.debugfs_tracepoint().unwrap();
        if buf.is_empty() {
            return Err(SystemError::EINVAL);
        }
        tracepoint.enable_file().write(buf[0] as _);
        Ok(buf.len())
    }

    fn poll(&self, _data: KernCallbackData) -> Result<PollStatus, SystemError> {
        Err(SystemError::ENOSYS)
    }
}

#[derive(Debug)]
struct FormatCallBack;

impl KernFSCallback for FormatCallBack {
    fn open(&self, _data: KernCallbackData) -> Result<(), SystemError> {
        Ok(())
    }

    fn read(
        &self,
        data: KernCallbackData,
        buf: &mut [u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        let pri_data = data.private_data().as_ref().unwrap();
        let tracepoint = pri_data.debugfs_tracepoint().unwrap();
        let format_str = tracepoint.format_file().read();
        if offset >= format_str.as_bytes().len() {
            return Ok(0); // Offset is beyond the length of the string
        }
        let len = buf.len().min(format_str.as_bytes().len() - offset);
        buf[..len].copy_from_slice(&format_str.as_bytes()[offset..offset + len]);
        Ok(len)
    }

    fn write(
        &self,
        _data: KernCallbackData,
        _buf: &[u8],
        _offset: usize,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EPERM)
    }

    fn poll(&self, _data: KernCallbackData) -> Result<PollStatus, SystemError> {
        Err(SystemError::ENOSYS)
    }
}

#[derive(Debug)]
struct IDCallBack;
impl KernFSCallback for IDCallBack {
    fn open(&self, _data: KernCallbackData) -> Result<(), SystemError> {
        Ok(())
    }

    fn read(
        &self,
        data: KernCallbackData,
        buf: &mut [u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        let pri_data = data.private_data().as_ref().unwrap();
        let tracepoint = pri_data.debugfs_tracepoint().unwrap();
        let id_str = tracepoint.id_file().read();

        if offset >= id_str.as_bytes().len() {
            return Ok(0); // Offset is beyond the length of the string
        }
        let len = buf.len().min(id_str.as_bytes().len() - offset);
        buf[..len].copy_from_slice(&id_str.as_bytes()[offset..offset + len]);
        Ok(len)
    }

    fn write(
        &self,
        _data: KernCallbackData,
        _buf: &[u8],
        _offset: usize,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EPERM)
    }

    fn poll(&self, _data: KernCallbackData) -> Result<PollStatus, SystemError> {
        Err(SystemError::ENOSYS)
    }
}

static mut TRACING_EVENTS_MANAGER: Option<TracingEventsManager> = None;

pub fn tracing_events_manager() -> &'static TracingEventsManager {
    unsafe {
        TRACING_EVENTS_MANAGER
            .as_ref()
            .expect("TracingEventsManager not initialized")
    }
}

pub fn init_events(root: Arc<KernFSInode>) -> Result<(), SystemError> {
    let events_manager = crate::tracepoint::global_init_events()?;
    // Register the global tracing events manager
    for subsystem_name in events_manager.subsystem_names() {
        let subsystem = events_manager.get_subsystem(&subsystem_name).unwrap();
        // Register the subsystem in the root inode
        let subsystem_inode = root.add_dir(
            subsystem_name,
            ModeType::from_bits_truncate(0o755),
            None,
            Some(&TracingDirCallBack),
        )?;
        for event_name in subsystem.event_names() {
            let event_info = subsystem.get_event(&event_name).unwrap();
            let event_inode = subsystem_inode.add_dir(
                event_name,
                ModeType::from_bits_truncate(0o755),
                None,
                Some(&TracingDirCallBack),
            )?;
            // add enable file for the event
            let _enable_inode = event_inode.add_file(
                "enable".to_string(),
                ModeType::from_bits_truncate(0o644),
                None,
                Some(KernInodePrivateData::DebugFS(event_info.clone())),
                Some(&EnableCallBack),
            )?;
            // add format file for the event
            let _format_inode = event_inode.add_file(
                "format".to_string(),
                ModeType::from_bits_truncate(0o644),
                None,
                Some(KernInodePrivateData::DebugFS(event_info.clone())),
                Some(&FormatCallBack),
            )?;
            // add id file for the event
            let _id_inode = event_inode.add_file(
                "id".to_string(),
                ModeType::from_bits_truncate(0o644),
                None,
                Some(KernInodePrivateData::DebugFS(event_info)),
                Some(&IDCallBack),
            )?;
        }
    }
    unsafe {
        TRACING_EVENTS_MANAGER = Some(events_manager);
    }
    Ok(())
}
