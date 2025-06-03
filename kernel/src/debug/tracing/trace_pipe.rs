use crate::filesystem::kernfs::callback::{KernCallbackData, KernFSCallback, KernInodePrivateData};
use crate::filesystem::kernfs::{KernFSInodeArgs, KernInodeType};
use crate::filesystem::vfs::syscall::ModeType;
use crate::filesystem::vfs::PollStatus;
use crate::tracepoint::{TraceEntryParser, TracePipeSnapshot};
use core::fmt::Debug;
use system_error::SystemError;

#[derive(Debug)]
pub struct TraceCallBack;

impl KernFSCallback for TraceCallBack {
    fn open(&self, mut data: KernCallbackData) -> Result<(), SystemError> {
        let pri_data = data.private_data_mut();
        let snapshot = super::TRACE_RAW_PIPE.lock().snapshot();
        pri_data.replace(KernInodePrivateData::TracePipe(snapshot));
        Ok(())
    }

    fn read(
        &self,
        mut data: KernCallbackData,
        buf: &mut [u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        let pri_data = data.private_data_mut().as_mut().unwrap();
        let snapshot = pri_data.tracepipe().unwrap();
        let default_fmt_str = TracePipeSnapshot::default_fmt_str();

        let managet = super::events::tracing_events_manager();
        let tracepint_map = managet.tracepoint_map();
        let trace_cmdline_cache = super::TRACE_CMDLINE_CACHE.lock();

        if offset >= default_fmt_str.len() {
            // read real trace data
            let mut copy_len = 0;
            let mut peek_flag = false;
            loop {
                if snapshot.is_empty() {
                    break; // No more records to read
                }
                if let Some(record) = snapshot.peek() {
                    let record_str =
                        TraceEntryParser::parse(&tracepint_map, &trace_cmdline_cache, record);
                    if copy_len + record_str.len() > buf.len() {
                        break; // Buffer is full
                    }
                    let len = record_str.len();
                    buf[copy_len..copy_len + len].copy_from_slice(record_str.as_bytes());
                    copy_len += len;
                    peek_flag = true;
                }
                if peek_flag {
                    snapshot.pop(); // Remove the record after reading
                }
                peek_flag = false;
            }
            return Ok(copy_len);
        }
        let len = buf.len().min(default_fmt_str.len() - offset);
        buf[..len].copy_from_slice(&default_fmt_str.as_bytes()[offset..offset + len]);
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
        Ok(PollStatus::READ)
    }
}

pub fn kernel_inode_provider() -> KernFSInodeArgs {
    KernFSInodeArgs {
        mode: ModeType::from_bits_truncate(0o444),
        callback: Some(&TraceCallBack),
        inode_type: KernInodeType::File,
        size: Some(4096),
        private_data: None,
    }
}

#[derive(Debug)]
pub struct TracePipeCallBack;

impl KernFSCallback for TracePipeCallBack {
    fn open(&self, _data: KernCallbackData) -> Result<(), SystemError> {
        Ok(())
    }

    fn read(
        &self,
        _data: KernCallbackData,
        buf: &mut [u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        let mut trace_raw_pipe = super::TRACE_RAW_PIPE.lock();
        let default_fmt_str = TracePipeSnapshot::default_fmt_str();

        let managet = super::events::tracing_events_manager();
        let tracepint_map = managet.tracepoint_map();
        let trace_cmdline_cache = super::TRACE_CMDLINE_CACHE.lock();

        if offset >= default_fmt_str.len() {
            // read real trace data
            let mut copy_len = 0;
            let mut peek_flag = false;
            loop {
                if trace_raw_pipe.is_empty() {
                    break; // No more records to read
                }
                if let Some(record) = trace_raw_pipe.peek() {
                    let record_str =
                        TraceEntryParser::parse(&tracepint_map, &trace_cmdline_cache, record);
                    if copy_len + record_str.len() > buf.len() {
                        break; // Buffer is full
                    }
                    let len = record_str.len();
                    buf[copy_len..copy_len + len].copy_from_slice(record_str.as_bytes());
                    copy_len += len;
                    peek_flag = true;
                }
                if peek_flag {
                    trace_raw_pipe.pop(); // Remove the record after reading
                }
                peek_flag = false;
            }
            return Ok(copy_len);
        }
        let len = buf.len().min(default_fmt_str.len() - offset);
        buf[..len].copy_from_slice(&default_fmt_str.as_bytes()[offset..offset + len]);
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
        Ok(PollStatus::READ)
    }
}
