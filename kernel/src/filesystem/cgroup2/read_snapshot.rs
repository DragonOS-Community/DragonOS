//! One text record per open description, with seq-file read positioning.

use alloc::vec::Vec;
use system_error::SystemError;

#[derive(Debug, Default)]
pub(super) struct TextSnapshot {
    bytes: Option<Vec<u8>>,
    next_offset: usize,
}

impl TextSnapshot {
    pub(super) fn read(
        &mut self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        render: impl FnOnce() -> Result<Vec<u8>, SystemError>,
        validate: impl FnOnce() -> Result<(), SystemError>,
    ) -> Result<usize, SystemError> {
        let want = len.min(buf.len());
        if want == 0 {
            return Ok(0);
        }
        let refreshed = self.bytes.is_none() || offset == 0 || offset != self.next_offset;
        if refreshed {
            // A failed refresh must not leave an older record available.
            self.bytes = None;
            self.next_offset = 0;
            self.bytes = Some(render()?);
        }
        let bytes = self.bytes.as_ref().expect("text record initialized");
        let start = offset.min(bytes.len());
        let count = want.min(bytes.len() - start);
        if count == 0 && (!refreshed || offset != 0) {
            // Like kernfs seq_start at EOF: recheck the source, without show
            // or consuming a newer event. Cached tails need no source entry.
            if let Err(error) = validate() {
                self.bytes = None;
                self.next_offset = 0;
                return Err(error);
            }
        }
        buf[..count].copy_from_slice(&bytes[start..start + count]);
        // count <= bytes.len() - offset whenever nonzero; no offset+len sum.
        self.next_offset = offset + count;
        Ok(count)
    }
}
