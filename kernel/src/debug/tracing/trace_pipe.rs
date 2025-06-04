use crate::filesystem::kernfs::callback::{KernCallbackData, KernFSCallback, KernInodePrivateData};
use crate::filesystem::kernfs::{KernFSInodeArgs, KernInodeType};
use crate::filesystem::vfs::syscall::ModeType;
use crate::filesystem::vfs::PollStatus;
use crate::libs::wait_queue::WaitQueue;
use crate::process::{ProcessFlags, ProcessManager};
use crate::sched::SchedMode;
use crate::tracepoint::{TraceEntryParser, TracePipeOps};
use core::fmt::Debug;
use system_error::SystemError;

fn common_trace_pipe_read(
    trace_buf: &mut dyn TracePipeOps,
    buf: &mut [u8],
) -> Result<usize, SystemError> {
    let manager = super::events::tracing_events_manager();
    let tracepint_map = manager.tracepoint_map();
    let trace_cmdline_cache = super::TRACE_CMDLINE_CACHE.lock();
    // read real trace data
    let mut copy_len = 0;
    let mut peek_flag = false;
    loop {
        if let Some(record) = trace_buf.peek() {
            let record_str = TraceEntryParser::parse(&tracepint_map, &trace_cmdline_cache, record);
            if copy_len + record_str.len() > buf.len() {
                break; // Buffer is full
            }
            let len = record_str.len();
            buf[copy_len..copy_len + len].copy_from_slice(record_str.as_bytes());
            copy_len += len;
            peek_flag = true;
        }
        if peek_flag {
            trace_buf.pop(); // Remove the record after reading
            peek_flag = false;
        } else {
            break; // No more records to read
        }
    }
    Ok(copy_len)
}

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

        let default_fmt_str = snapshot.default_fmt_str();
        if offset >= default_fmt_str.len() {
            common_trace_pipe_read(snapshot, buf)
        } else {
            let len = buf.len().min(default_fmt_str.len() - offset);
            buf[..len].copy_from_slice(&default_fmt_str.as_bytes()[offset..offset + len]);
            Ok(len)
        }
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

static TracePipeCallBackWaitQueue: WaitQueue = WaitQueue::default();

#[derive(Debug)]
pub struct TracePipeCallBack;

impl TracePipeCallBack {
    fn readable(&self) -> bool {
        let trace_raw_pipe = super::TRACE_RAW_PIPE.lock();
        !trace_raw_pipe.is_empty()
    }
}

impl KernFSCallback for TracePipeCallBack {
    fn open(&self, _data: KernCallbackData) -> Result<(), SystemError> {
        Ok(())
    }

    fn read(
        &self,
        data: KernCallbackData,
        buf: &mut [u8],
        _offset: usize,
    ) -> Result<usize, SystemError> {
        drop(data); // We don't need the data here, release the internal lock
        let read_len = loop {
            let mut trace_raw_pipe = super::TRACE_RAW_PIPE.lock();
            let read_len = common_trace_pipe_read(&mut *trace_raw_pipe, buf).unwrap();
            if read_len != 0 {
                break read_len;
            }
            // Release the lock before waiting
            drop(trace_raw_pipe);
            // wait for new data
            let r = wq_wait_event_interruptible!(TracePipeCallBackWaitQueue, self.readable(), {});
            if r.is_err() {
                ProcessManager::current_pcb()
                    .flags()
                    .insert(ProcessFlags::HAS_PENDING_SIGNAL);
                return Err(SystemError::ERESTARTSYS);
            }
            // todo!(wq_wait_event_interruptible may has a bug)
        };
        Ok(read_len)
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
