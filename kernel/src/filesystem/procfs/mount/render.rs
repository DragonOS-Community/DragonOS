use alloc::{string::String, vec::Vec};

use system_error::SystemError;

use crate::filesystem::vfs::mount::with_topology_snapshot;

use super::{
    collect::{collect_mount_candidates, ProcMountEntry},
    fields::MountProcFields,
    format::{mountinfo_line, mounts_line, mountstats_line},
    MountView,
};

#[derive(Clone, Copy, Debug)]
pub(crate) enum ProcMountRenderKind {
    Mounts,
    MountInfo,
    MountStats,
}

/// Renders one slice of the mount-family record (`mounts` / `mountinfo` /
/// `mountstats`) from `view`.
///
/// Linux serves these through `seq_open_private()` (`fs/proc_namespace.c`): the
/// record is produced by the reader, not at open time, so a file opened and read
/// much later shows the topology the namespace reached by the time it is read —
/// within the namespace and root directory `mounts_open_common()` pinned at open.
/// One visible mount is one record, and a read hands out the slice of records
/// that fits the `seq_file` buffer, so an fd holds one output block rather than
/// the whole table.
///
/// `cursor` is the mount id the table was rendered up to (`None` starts a
/// record), and only mounts above it are reached: mount ids are allocated in
/// increasing order and never reused (`MountId::alloc()`), so a mount created
/// after an earlier slice is always reported by a later one. The id of the last
/// mount this slice reached is returned as the cursor to resume from, or `None`
/// when this slice reached the end of the table. A mount the pinned root does
/// not reach is passed over rather than rendered, and passing over it still
/// advances the cursor, the way `seq_path_root()` makes `show_vfsmnt()` return
/// without emitting a record.
///
/// The mounts the slice does not render are still enumerated, because the mount
/// tree is what says which mounts the namespace has and the table is ordered by
/// a key only a mount carries; the walk behind
/// [`collect_mount_candidates()`] costs no record fields for them, so one slice
/// costs one enumeration of the namespace's mounts -- a walk plus a sort, taken
/// under the global mount lifecycle snapshot, and with one transient list per
/// mount it visits -- plus a record for each mount it hands out.  A reader that
/// drains the whole table pays that enumeration once per slice, so its cost
/// grows with the table times the number of slices, while what the fd itself
/// keeps between reads stays one output block.
///
/// Linux's `m_start()` resumes an iteration instead of re-deriving it, which
/// needs the ordered mount list (`ns->list`) plus the cursor node an fd keeps
/// linked into it (`fs/namespace.c`); the namespace here keeps mounts in a tree,
/// so the order is re-derived per slice.  An index that would spare the walk (a
/// second, id-ordered copy of the namespace's mounts) would have to be kept in
/// step by every attach, detach, copy and propagation path, so the table pays
/// the walk instead of adding a second source of truth for what the namespace
/// contains.
///
/// `budget` bounds the slice to roughly that many bytes: the renderer stops
/// after the record that crossed the bound, so the result is one page plus at
/// most one record, the same way `seq_read_iter()` refills `m->buf` and grows it
/// only for a single record that cannot fit.
///
/// `out` is where the slice is appended: the caller hands in the buffer of a
/// slice that was just emptied, and the record a slice stops on stays in it
/// while the returned cursor marks the reader's place in the table.
pub(crate) fn render_mount_slice(
    view: &MountView,
    kind: ProcMountRenderKind,
    cursor: Option<usize>,
    budget: usize,
    out: &mut Vec<u8>,
) -> Result<Option<usize>, SystemError> {
    // The candidates are enumerated from one topology snapshot, and each record
    // the slice hands out resolves its own place in the topology under a
    // snapshot of its own: see `ProcMountEntry::resolve()`.  Only the records
    // that fit the slice are resolved, so a slice covers one output block rather
    // than the whole table.  The enumeration itself has to stay in the snapshot:
    // an edge commit such as `mount --move` publishes its two parents' mount
    // point maps one after the other, so an unlocked walk could meet the same
    // mount through both of them and list it twice.
    let candidates = with_topology_snapshot(|| collect_mount_candidates(view, cursor))?;
    let mut record = String::new();

    for (index, candidate) in candidates.iter().enumerate() {
        // A mount the slice passes over is still a mount the reader's table has
        // reached: the cursor is the reader's place in the table, not the
        // number of records it has been handed.
        let Some(entry) = ProcMountEntry::resolve(candidate, view)? else {
            continue;
        };
        record.clear();
        let fields = MountProcFields::from_entry(&entry)?;
        match kind {
            ProcMountRenderKind::Mounts => mounts_line::render(&fields, &mut record)?,
            ProcMountRenderKind::MountInfo => mountinfo_line::render(&fields, &mut record)?,
            ProcMountRenderKind::MountStats => mountstats_line::render(&fields, &mut record)?,
        }
        out.extend_from_slice(record.as_bytes());
        if out.len() >= budget {
            // A record that crossed the bound is handed out with this slice;
            // only a slice that has nothing left to reach ends the record.
            return Ok(if index + 1 < candidates.len() {
                Some(candidate.mount_id)
            } else {
                None
            });
        }
    }

    Ok(None)
}
