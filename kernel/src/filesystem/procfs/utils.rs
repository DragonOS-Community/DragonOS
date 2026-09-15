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
/// resumes from. See [`proc_read_seq()`].
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
/// arbitrarily much, such as the mapping table of `/proc/[pid]/maps`.
///
/// Only an incremental source is held to it: the sources that render one whole
/// record ([`proc_read_snapshot()`], i.e. Linux `single_open()`) put that record
/// in the fd's buffer in one piece, which is what makes an fd a snapshot of it.
const SEQ_SLICE_MAX: usize = MMArch::PAGE_SIZE;

/// Serves one procfs record the way Linux `seq_read_iter()` does.
///
/// The fd holds the bytes of the slice the source produced plus an opaque resume
/// cursor, and the source is entered only when that buffer runs dry. A record
/// that changes in between therefore cannot tear the byte stream, and once the
/// source reports the end of the record the fd keeps reporting EOF.
///
/// `source` is handed the cursor of the previous slice (`None` for the first
/// slice of a record), how many bytes the reader still wants, and an empty
/// buffer to render into. The wanted byte count is a budget, never more than
/// [`SEQ_SLICE_MAX`]: an incremental source stops after roughly that much and is
/// entered again for the next slice, so one read may need several slices, while
/// a source that renders one whole record ignores the budget on purpose. The
/// source returns the cursor to resume from, or `None` when the record ends
/// after this slice.
///
/// Position rules mirror `seq_read_iter()`/`seq_lseek()`:
/// - `offset == 0`: rewind, so the record is rendered again;
/// - `offset == seq.offset`: continue; the source is not entered while buffered
///   bytes remain, even when the target is already gone;
/// - any other offset: seek, so the record is rendered again from the start and
///   the first `offset` bytes are dropped.
/// - a seek past the end of the record parks the fd at the requested offset, so
///   reading there reports EOF instead of rendering the record again.
///
/// An empty request (`len == 0`, or a full buffer) returns 0 without rendering
/// anything, and a source that fails after this call already copied bytes out
/// still reports those bytes, as `seq_read_iter()` returns `copied` and discards
/// the error once it copied something.
pub(super) fn proc_read_seq<F>(
    offset: usize,
    len: usize,
    buf: &mut [u8],
    data: &mut MutexGuard<FilePrivateData>,
    mut source: F,
) -> Result<usize, SystemError>
where
    F: FnMut(Option<usize>, usize, &mut Vec<u8>) -> Result<Option<usize>, SystemError>,
{
    // A read that reaches `read_at()` without the state `ProcFile::open()` left
    // behind is not a procfs read. Serving it from state that lives no longer
    // than this call would render and slice the record per read, which is the
    // tear this driver exists to prevent, so it is refused instead.
    let FilePrivateData::Procfs(pdata) = &mut **data else {
        return Err(SystemError::EINVAL);
    };
    let seq = &mut pdata.seq;

    // `seq_read_iter()` returns before it touches the iterator when the request
    // is empty, and `copy_to_iter()` never copies more than the iovec holds.
    let len = len.min(buf.len());
    if len == 0 {
        return Ok(0);
    }

    let mut skip = 0;
    // A read at offset 0 rewinds even when the fd already sits there, because
    // `seq_read_iter()` resets `m->index`/`m->count` on every `ki_pos == 0`
    // read: a record that was empty when the fd was first read has to be
    // rendered again rather than replaying that EOF forever.
    if offset == 0 || seq.offset != offset {
        seq.reset();
        skip = offset;
    }

    let mut written = 0;
    loop {
        if written >= len {
            // The reader is satisfied, and `seq_read_iter()` likewise stops
            // filling once the iterator has no room left.
            break;
        }
        if seq.available() == 0 {
            if seq.exhausted {
                break;
            }
            seq.rendered.clear();
            seq.taken = 0;
            let want = (len - written).min(SEQ_SLICE_MAX);
            let resume = match source(seq.cursor, want, &mut seq.rendered) {
                Ok(resume) => resume,
                Err(err) => {
                    // A slice that could not be produced is not part of the
                    // record: drop whatever it wrote and keep the cursor, so a
                    // later read retries that record. Bytes this call already
                    // copied out still count, the way `seq_read_iter()` returns
                    // `copied` and drops `err` once it copied something.
                    seq.rendered.clear();
                    seq.taken = 0;
                    if written == 0 {
                        return Err(err);
                    }
                    return Ok(written);
                }
            };
            seq.cursor = resume;
            // A source that produced nothing cannot make progress, so its slice
            // is treated as the end of the record instead of being re-entered.
            seq.exhausted = resume.is_none() || seq.rendered.is_empty();
            if seq.available() == 0 {
                break;
            }
        }

        if skip > 0 {
            let dropped = skip.min(seq.available());
            seq.discard(dropped);
            skip -= dropped;
            continue;
        }

        // `available() > 0` and `written < len <= buf.len()`, so this slice is
        // in bounds and never empty.
        let take = seq.available().min(len - written);
        seq.take_into(&mut buf[written..written + take]);
        written += take;
    }

    if skip > 0 {
        // The seek ran off the end of the record, so no byte of it is pending.
        // Linux `traverse()` still records `m->read_pos = iocb->ki_pos`, which is
        // what keeps a later read at that position from rendering again.
        seq.offset = offset;
    }
    return Ok(written);
}

/// Serves a procfs record that a file renders in one piece, the way Linux
/// `single_open()` files are read.
///
/// `render` is entered on the first read of an fd and again only when that fd is
/// rewound or seeked. While the fd still holds bytes of its record, a later
/// `read()` replays them, so content that grows in between neither tears the
/// byte stream nor revives EOF.
pub(super) fn proc_read_snapshot<F>(
    offset: usize,
    len: usize,
    buf: &mut [u8],
    data: &mut MutexGuard<FilePrivateData>,
    mut render: F,
) -> Result<usize, SystemError>
where
    F: FnMut() -> Result<Vec<u8>, SystemError>,
{
    proc_read_seq(offset, len, buf, data, move |cursor, _want, out| {
        // `None` is the start of a record: first read, rewind or seek. Any other
        // cursor means the whole record already went out in the first slice.
        if cursor.is_none() {
            *out = render()?;
        }
        Ok(None)
    })
}
