use crate::filesystem::kernfs::callback::{KernCallbackData, KernFSCallback};
use crate::filesystem::vfs::PollStatus;
use crate::libs::spinlock::SpinLock;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Debug;
use system_error::SystemError;

static mut TRACE_PIPE: Option<TracePipe> = None;

pub const TRACE_PIPE_MAX_RECORD: usize = 4096;

pub fn init_trace_pipe() {
    unsafe {
        TRACE_PIPE = Some(TracePipe::new(TRACE_PIPE_MAX_RECORD));
    }
}

pub fn trace_pipe() -> &'static TracePipe {
    unsafe { TRACE_PIPE.as_ref().unwrap() }
}

/// Push a record to trace pipe
pub fn trace_pipe_push_record(record: String) {
    trace_pipe().push_record(record);
}

pub struct TracePipe {
    buf: SpinLock<TracePipeBuf>,
}

struct TracePipeBuf {
    size: usize,
    max_record: usize,
    buf: Vec<String>,
}

impl TracePipeBuf {
    pub const fn new(max_record: usize) -> Self {
        Self {
            max_record,
            size: 0,
            buf: Vec::new(),
        }
    }

    pub fn push_str(&mut self, record: String) {
        let record_size = record.len();
        if self.size + record_size > self.max_record {
            let mut i = 0;
            while i < record_size {
                let t = self.buf.pop().unwrap();
                self.size -= t.len();
                i += t.len();
            }
        }
        self.buf.push(record);
        self.size += record_size;
    }

    pub fn read_at(&self, buf: &mut [u8], offset: usize) -> Result<usize, SystemError> {
        if offset == self.size {
            return Ok(0);
        }
        if buf.len() < self.size {
            return Err(SystemError::EINVAL);
        }
        let mut count = 0;
        for line in self.buf.iter() {
            let line = line.as_bytes();
            buf[count..count + line.len()].copy_from_slice(line);
            count += line.len();
        }
        Ok(count)
    }
}

impl TracePipe {
    pub const fn new(max_record: usize) -> Self {
        Self {
            buf: SpinLock::new(TracePipeBuf::new(max_record)),
        }
    }
    pub fn push_record(&self, record: String) {
        self.buf.lock().push_str(record);
    }

    pub fn read_at(&self, buf: &mut [u8], offset: usize) -> Result<usize, SystemError> {
        self.buf.lock().read_at(buf, offset)
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
        let trace_pipe = trace_pipe();
        trace_pipe.read_at(buf, offset)
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
