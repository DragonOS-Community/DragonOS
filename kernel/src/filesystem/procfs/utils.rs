use alloc::vec::Vec;
use system_error::SystemError;

use crate::{filesystem::vfs::FilePrivateData, libs::mutex::MutexGuard};

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

/// Snapshot read for procfs: mirrors Linux `seq_file` (`single_open()` /
/// `seq_read_iter()` / `seq_lseek()`).
///
/// One fd re-renders only on the first read, or when the read position no longer
/// matches the continuation position:
///
/// - `offset == read_pos` and `offset != 0`: continue from the snapshot, so
///   **content growing between two reads does not revive EOF**;
/// - `offset == 0`: re-render, matching `seq_read_iter()`'s `ki_pos == 0` reset;
/// - anything else: re-render and reposition at `offset`, matching `traverse()`.
///
/// `render` is only called when needed (the continuation path never re-renders).
/// A failed render drops the continuation point, the way a failed `traverse()`
/// resets the buffer (`fs/seq_file.c:196-203`), so the next read re-renders
/// instead of serving a stale snapshot.
pub(super) fn proc_read_snapshot<F>(
    offset: usize,
    len: usize,
    buf: &mut [u8],
    data: &mut MutexGuard<FilePrivateData>,
    render: F,
) -> Result<usize, SystemError>
where
    F: FnOnce() -> Result<Vec<u8>, SystemError>,
{
    let FilePrivateData::Procfs(pdata) = &mut **data else {
        // A few callers (e.g. symlink reads) reach read_at() without procfs
        // private data. Fall back to render-per-read, i.e. the previous behaviour.
        let content = render()?;
        return proc_read(offset, len, buf, &content);
    };

    let pos = match pdata.read_pos {
        Some(pos) if pos == offset && offset != 0 => pos,
        _ => {
            match render() {
                Ok(rendered) => {
                    pdata.data = rendered;
                    pdata.read_pos = Some(offset);
                }
                Err(err) => {
                    pdata.read_pos = None;
                    return Err(err);
                }
            }
            offset
        }
    };

    let n = proc_read(pos, len, buf, &pdata.data)?;
    pdata.read_pos = Some(pos + n);
    Ok(n)
}
