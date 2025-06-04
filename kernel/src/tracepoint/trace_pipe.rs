use crate::tracepoint::{TraceEntry, TracePointMap};
use alloc::{format, string::String, vec::Vec};

pub trait TracePipeOps {
    /// Returns the first event in the trace pipe buffer without removing it.
    fn peek(&self) -> Option<&Vec<u8>>;

    /// Remove and return the first event in the trace pipe buffer.
    fn pop(&mut self) -> Option<Vec<u8>>;

    /// Whether the trace pipe buffer is empty.
    fn is_empty(&self) -> bool;
}

/// A raw trace pipe buffer that stores trace events as byte vectors.
pub struct TracePipeRaw {
    max_record: usize,
    event_buf: Vec<Vec<u8>>,
}

impl TracePipeRaw {
    pub const fn new(max_record: usize) -> Self {
        Self {
            max_record,
            event_buf: Vec::new(),
        }
    }

    /// Set the maximum number of records to keep in the trace pipe buffer.
    ///
    /// If the current number of records exceeds this limit, the oldest records will be removed.
    pub fn set_max_record(&mut self, max_record: usize) {
        self.max_record = max_record;
        if self.event_buf.len() > max_record {
            self.event_buf.truncate(max_record); // Keep only the latest records
        }
    }

    /// Push a new event into the trace pipe buffer.
    pub fn push_event(&mut self, event: Vec<u8>) {
        if self.event_buf.len() >= self.max_record {
            self.event_buf.remove(0); // Remove the oldest record
        }
        self.event_buf.push(event);
    }

    /// The number of events currently in the trace pipe buffer.
    pub fn event_count(&self) -> usize {
        self.event_buf.len()
    }

    /// Clear the trace pipe buffer.
    pub fn clear(&mut self) {
        self.event_buf.clear();
    }

    /// Create a snapshot of the current state of the trace pipe buffer.
    pub fn snapshot(&self) -> TracePipeSnapshot {
        TracePipeSnapshot::new(self.event_buf.clone())
    }
}

impl TracePipeOps for TracePipeRaw {
    fn peek(&self) -> Option<&Vec<u8>> {
        self.event_buf.first()
    }

    fn pop(&mut self) -> Option<Vec<u8>> {
        if self.event_buf.is_empty() {
            None
        } else {
            Some(self.event_buf.remove(0))
        }
    }

    fn is_empty(&self) -> bool {
        self.event_buf.is_empty()
    }
}

#[derive(Debug)]
pub struct TracePipeSnapshot(Vec<Vec<u8>>);

impl TracePipeSnapshot {
    pub fn new(event_buf: Vec<Vec<u8>>) -> Self {
        Self(event_buf)
    }

    /// The formatted string representation to be used as a header for the trace pipe output.
    pub fn default_fmt_str(&self) -> String {
        let show = "#
#
#                                _-----=> irqs-off/BH-disabled
#                               / _----=> need-resched
#                              | / _---=> hardirq/softirq
#                              || / _--=> preempt-depth
#                              ||| / _-=> migrate-disable
#                              |||| /     delay
#           TASK-PID     CPU#  |||||  TIMESTAMP  FUNCTION
#              | |         |   |||||     |         |
";
        format!(
            "# tracer: nop\n#\n# entries-in-buffer/entries-written: {}/{}   #P:32\n{}",
            self.0.len(),
            self.0.len(),
            show
        )
    }
}

impl TracePipeOps for TracePipeSnapshot {
    fn peek(&self) -> Option<&Vec<u8>> {
        self.0.first()
    }

    fn pop(&mut self) -> Option<Vec<u8>> {
        if self.0.is_empty() {
            None
        } else {
            Some(self.0.remove(0))
        }
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A cache for storing command line arguments for each trace point.
///
/// See https://www.kernel.org/doc/Documentation/trace/ftrace.txt
pub struct TraceCmdLineCache {
    cmdline: Vec<(u32, [u8; 16])>,
    max_record: usize,
}

impl TraceCmdLineCache {
    pub const fn new(max_record: usize) -> Self {
        Self {
            cmdline: Vec::new(),
            max_record,
        }
    }

    /// Insert a command line argument for a trace point.
    ///
    /// If the command line exceeds 16 bytes, it will be truncated.
    /// If the cache exceeds the maximum record limit, the oldest entry will be removed.
    pub fn insert(&mut self, id: u32, cmdline: String) {
        if self.cmdline.len() >= self.max_record {
            // Remove the oldest entry if we exceed the max record limit
            self.cmdline.remove(0);
        }
        let mut cmdline_bytes = [0u8; 16];
        if cmdline.len() > 16 {
            // Truncate to fit the fixed size
            cmdline_bytes.copy_from_slice(&cmdline.as_bytes()[..16]);
        } else {
            // Copy the command line bytes into the fixed size array
            cmdline_bytes[..cmdline.len()].copy_from_slice(cmdline.as_bytes());
        }
        self.cmdline.push((id, cmdline_bytes));
    }

    /// Get the command line argument for a trace point.
    pub fn get(&self, id: u32) -> Option<&str> {
        self.cmdline.iter().find_map(|(key, value)| {
            if *key == id {
                Some(core::str::from_utf8(value).unwrap().trim_end_matches('\0'))
            } else {
                None
            }
        })
    }

    /// Set the maximum length for command line arguments.
    pub fn set_max_record(&mut self, max_len: usize) {
        self.max_record = max_len;
        if self.cmdline.len() > max_len {
            self.cmdline.truncate(max_len); // Keep only the latest records
        }
    }
}

pub struct TraceEntryParser;

impl TraceEntryParser {
    /// Parse the trace entry and return a formatted string.
    pub fn parse(
        tracepoint_map: &TracePointMap,
        cmdline_cache: &TraceCmdLineCache,
        entry: &[u8],
    ) -> String {
        let trace_entry = unsafe { &*(entry.as_ptr() as *const TraceEntry) };
        let id = trace_entry.type_ as u32;
        let tracepoint = tracepoint_map.get(&id).expect("TracePoint not found");
        let fmt_func = tracepoint.fmt_func();
        let offset = core::mem::size_of::<TraceEntry>();
        let str = fmt_func(&entry[offset..]);

        let time = crate::time::Instant::now().total_micros() * 1000; // Convert to nanoseconds
        let cpu_id = crate::arch::cpu::current_cpu_id().data();

        // Copy the packed field to a local variable to avoid unaligned reference
        let pid = trace_entry.pid;
        let pname = cmdline_cache.get(pid as u32).unwrap_or("<...>");

        let secs = time / 1_000_000_000;
        let usec_rem = time % 1_000_000_000 / 1000;

        format!(
            "{:>16}-{:<7} [{:03}] {} {:5}.{:06}: {}({})\n",
            pname,
            pid,
            cpu_id,
            trace_entry.trace_print_lat_fmt(),
            secs,
            usec_rem,
            tracepoint.name(),
            str
        )
    }
}
