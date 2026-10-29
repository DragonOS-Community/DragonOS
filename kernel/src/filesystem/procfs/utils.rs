use alloc::vec::Vec;
use system_error::SystemError;

use crate::{
    arch::MMArch, filesystem::vfs::FilePrivateData, libs::mutex::MutexGuard,
    mm::MemoryManagementArch,
};

/// 去除Vec中所有的\0,并在结尾添加\0
#[inline]
pub(super) fn trim_string(data: &mut Vec<u8>) {
    data.retain(|x| *x != 0);
    data.push(0);
}

/// proc文件系统读取函数
pub(super) fn proc_read(
    offset: usize,
    len: usize,
    buf: &mut [u8],
    data: &[u8],
) -> Result<usize, SystemError> {
    let start = data.len().min(offset);
    let end = data.len().min(offset + len);

    // buffer空间不足
    if buf.len() < (end - start) {
        return Err(SystemError::ENOBUFS);
    }

    // 拷贝数据
    let src = &data[start..end];
    buf[0..src.len()].copy_from_slice(src);
    return Ok(src.len());
}

/// State of a seq-style procfs fd (Linux `struct seq_file`).
///
/// The fd keeps the bytes a record source produced but the reader has not
/// drained yet, where those bytes sit in the file, and the cursor the source
/// resumes from. See [`proc_seq_operation()`].
#[derive(Debug, Clone, Default)]
pub(crate) struct ProcfsSeq {
    /// Slice produced by the record source that the reader has not drained.
    rendered: Vec<u8>,
    /// How many bytes of `rendered` the reader already copied out.
    taken: usize,
    /// File offset that `rendered[taken]` maps to.
    offset: usize,
    /// Where the record source resumes; `None` before its first slice.
    cursor: Option<usize>,
    /// The source reported the end of the record.
    exhausted: bool,
}

impl ProcfsSeq {
    pub(super) fn read_position(&self) -> usize {
        self.offset
    }

    pub(super) fn rewind(&mut self) {
        self.reset();
    }

    /// Bytes of the current slice that are still to be copied out.
    fn available(&self) -> usize {
        self.rendered.len() - self.taken
    }

    /// Drops the buffered slice and the cursor, so the next slice starts over at
    /// the beginning of the record (Linux `traverse()`).
    fn reset(&mut self) {
        // Replacing the buffer, rather than clearing it, releases a slice that
        // had grown large before the rewind.
        self.rendered = Vec::new();
        self.taken = 0;
        self.offset = 0;
        self.cursor = None;
        self.exhausted = false;
    }

    /// Copies the buffered bytes out and advances the file position.
    fn take_into(&mut self, buf: &mut [u8]) {
        let end = self.taken + buf.len();
        buf.copy_from_slice(&self.rendered[self.taken..end]);
        self.taken = end;
        self.offset += buf.len();
    }

    /// Drops buffered bytes without copying them out, for a seek over bytes the
    /// reader never asked for.
    fn discard(&mut self, count: usize) {
        self.taken += count;
        self.offset += count;
    }
}

/// Most bytes one slice of a record may render into an fd's buffer.
///
/// Linux `seq_file` starts with one page (`m->size = PAGE_SIZE`) and only grows
/// the buffer for a single record that cannot fit in it
/// (`fs/seq_file.c:seq_read_iter()`), so a reader's read length does not decide
/// how much a seq file buffers. Bounding a slice the same way keeps a large read
/// from turning into a large per-fd buffer for a record source that can render
/// arbitrarily much, such as the mapping table of `/proc/[pid]/maps` or the
/// mount table of `/proc/[pid]/mountinfo`.
///
/// Only an incremental source is held to it: the sources that render one whole
/// record ([`proc_snapshot_operation()`], i.e. Linux `single_open()`) put that record
/// in the fd's buffer in one piece, which is what makes an fd a snapshot of it.
const SEQ_SLICE_MAX: usize = MMArch::PAGE_SIZE;

/// A provider supplies the same record source for reads and real seq seeks.
/// `Seek` is a byte position; the provider cursor remains opaque.
pub enum ProcfsSeqOperation<'a> {
    Read {
        offset: usize,
        len: usize,
        buf: &'a mut [u8],
    },
    Seek {
        target: usize,
    },
}

impl ProcfsSeq {
    /// Produce one slice. A failed slice must never leave partially rendered
    /// bytes visible; the source cursor remains at the failed record.
    fn fill<F>(&mut self, budget: usize, source: &mut F) -> Result<(), SystemError>
    where
        F: FnMut(Option<usize>, usize, &mut Vec<u8>) -> Result<Option<usize>, SystemError>,
    {
        self.rendered.clear();
        self.taken = 0;
        match source(self.cursor, budget, &mut self.rendered) {
            Ok(resume) => {
                self.cursor = resume;
                self.exhausted = resume.is_none() || self.rendered.is_empty();
                Ok(())
            }
            Err(err) => {
                self.rendered.clear();
                Err(err)
            }
        }
    }

    /// Linux traverse(): start over and stop in the record containing target,
    /// or exactly at a record boundary without prefetching another record.
    /// Budget one asks incremental providers for one record, while single-open
    /// providers naturally render their one complete record.
    fn traverse<F>(&mut self, target: usize, source: &mut F) -> Result<(), SystemError>
    where
        F: FnMut(Option<usize>, usize, &mut Vec<u8>) -> Result<Option<usize>, SystemError>,
    {
        self.reset();
        while self.offset < target {
            if self.available() == 0 {
                if self.exhausted {
                    self.offset = target;
                    break;
                }
                if let Err(err) = self.fill(1, source) {
                    self.reset();
                    return Err(err);
                }
                if self.available() == 0 {
                    self.offset = target;
                    break;
                }
            }
            self.discard((target - self.offset).min(self.available()));
        }
        Ok(())
    }
}

/// Serve the provider's record source with per-open seq state. Reads preserve
/// the ordinary demand bound; seeks traverse one record at a time and capture
/// the bytes at the seek syscall, rather than delaying work until the read.
pub(super) fn proc_seq_operation<F>(
    operation: ProcfsSeqOperation<'_>,
    data: &mut MutexGuard<FilePrivateData>,
    mut source: F,
) -> Result<usize, SystemError>
where
    F: FnMut(Option<usize>, usize, &mut Vec<u8>) -> Result<Option<usize>, SystemError>,
{
    let FilePrivateData::Procfs(pdata) = &mut **data else {
        return Err(SystemError::EINVAL);
    };
    let seq = &mut pdata.seq;
    let (offset, len, buf) = match operation {
        ProcfsSeqOperation::Seek { target } => {
            if target != seq.offset {
                seq.traverse(target, &mut source)?;
            }
            return Ok(target);
        }
        ProcfsSeqOperation::Read { offset, len, buf } => (offset, len.min(buf.len()), buf),
    };
    // Empty reads do not sample the provider or move its cursor.
    if len == 0 {
        return Ok(0);
    }
    if offset == 0 {
        // seq_read_iter resets even at a previously observed empty record.
        seq.reset();
    } else if offset != seq.offset {
        seq.traverse(offset, &mut source)?;
    }
    let mut written = 0;
    while written < len {
        if seq.available() == 0 {
            if seq.exhausted {
                break;
            }
            if let Err(err) = seq.fill((len - written).min(SEQ_SLICE_MAX), &mut source) {
                return if written == 0 { Err(err) } else { Ok(written) };
            }
            if seq.available() == 0 {
                break;
            }
        }
        let take = seq.available().min(len - written);
        seq.take_into(&mut buf[written..written + take]);
        written += take;
    }
    Ok(written)
}

/// Single-open providers render their one record in full. The operation and
/// buffering rules remain shared with incremental seq providers.
pub(super) fn proc_snapshot_operation<F>(
    operation: ProcfsSeqOperation<'_>,
    data: &mut MutexGuard<FilePrivateData>,
    mut render: F,
) -> Result<usize, SystemError>
where
    F: FnMut() -> Result<Vec<u8>, SystemError>,
{
    proc_seq_operation(operation, data, move |cursor, _want, out| {
        if cursor.is_none() {
            *out = render()?;
        }
        Ok(None)
    })
}
