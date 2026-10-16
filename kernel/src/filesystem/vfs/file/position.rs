//! Sleepable current-position serialization belongs to the open description,
//! not the descriptor number or inode. Positional I/O never takes this lock.

use super::{File, FileMode, ReaddirState};
use crate::libs::mutex::MutexGuard;

impl File {
    /// Hold across position validation, all I/O chunks, user copy and commit.
    /// Never acquire while holding a file-table lock. Pipe transfers must finish
    /// their readiness wait/reservation before acquiring the file position.
    pub(crate) fn lock_position(&self) -> Option<MutexGuard<'_, ReaddirState>> {
        let mode = self.mode();
        (mode.contains(FileMode::FMODE_ATOMIC_POS) && !mode.contains(FileMode::FMODE_STREAM))
            .then(|| self.position_state.lock())
    }

    /// Lock only implicit-position participants, in stable OFD identity order.
    /// Aliases of the same description need one guard, regardless of fd numbers.
    pub(crate) fn lock_position_pair<'a>(
        first: Option<&'a File>,
        second: Option<&'a File>,
    ) -> (
        Option<MutexGuard<'a, ReaddirState>>,
        Option<MutexGuard<'a, ReaddirState>>,
    ) {
        match (first, second) {
            (Some(a), Some(b)) if a.open_file_id == b.open_file_id => (a.lock_position(), None),
            (Some(a), Some(b)) if a.open_file_id > b.open_file_id => {
                (b.lock_position(), a.lock_position())
            }
            (a, b) => (
                a.and_then(File::lock_position),
                b.and_then(File::lock_position),
            ),
        }
    }
}
