use alloc::{string::String, vec::Vec};

use system_error::SystemError;

use crate::filesystem::vfs::mount::with_topology_snapshot;

use super::{
    collect::{collect_mount_candidates, ProcMountEntry, VisibleMount},
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

// The shortest possible record is the `mounts` format with four empty fields:
// three field separators plus " 0 0\n". Escaping only expands path tokens, so
// their original lengths can tighten this bound without calling a filesystem.
const MIN_MOUNT_LINE_BYTES: usize = 8;

fn minimum_line_len(mount: &VisibleMount, kind: ProcMountRenderKind) -> usize {
    let path_len = mount.mountpoint_display.len();
    match kind {
        ProcMountRenderKind::Mounts => MIN_MOUNT_LINE_BYTES
            .saturating_add(path_len)
            .saturating_add(mount.per_mount_options.len()),
        ProcMountRenderKind::MountInfo => MIN_MOUNT_LINE_BYTES
            .saturating_add(path_len)
            .saturating_add(mount.mountinfo_root.len())
            .saturating_add(mount.per_mount_options.len())
            .saturating_add(mount.super_block_options.len())
            .saturating_add(mount.mountinfo_tags.len()),
        ProcMountRenderKind::MountStats => MIN_MOUNT_LINE_BYTES.saturating_add(path_len),
    }
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
/// The cursor is a watermark over the table, not a re-derivation of it: a mount
/// whose id a slice has already reached is never handed out again, even when it
/// becomes reachable afterwards (an `MS_MOVE` that brings an existing mount
/// under the pinned root keeps that mount's id). Linux's cursor node has the
/// same shape -- `m_stop()` moves it after the last mount it emitted
/// (`fs/namespace.c`), and an `MS_MOVE` keeps the mount's place in `ns->list`
/// (`attach_recursive_mnt()` inserts only a newly attached mount at the tail),
/// so a mount the iterator already passed is likewise not reported again.
///
/// The mounts the slice does not render are still enumerated, because the mount
/// tree is what says which mounts the namespace has and the table is ordered by
/// a key only a mount carries; the walk behind
/// [`collect_mount_candidates()`] costs no record fields for them. One slice
/// enumerates and sorts the candidates, then captures enough visible mounts
/// to meet the output budget by a conservative lower bound of each formatted
/// line, under the same topology snapshot. The
/// filesystem callbacks run after releasing that snapshot. A
/// reader that drains the table pays one enumeration per slice, while the fd
/// itself keeps only one output block.
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
/// only for a single record that cannot fit. The driver also bounds the slice by
/// what the reader asked for (`proc_read_seq()`), so a reader that asks for one
/// byte takes a one-record slice and pays one enumeration per record, while one
/// that asks for a page or more pays one per output block.
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
    // Linux m_start()/m_stop() holds namespace_sem across a seq buffer's
    // records. Capture the topology fields for this buffer under one snapshot:
    // an MS_REC propagation change must not put different generations in two
    // lines of one read. The selected Vec lives outside the closure so an error
    // releases the topology lock before dropping any superblock snapshot pins.
    let mut selected = Vec::new();
    let mut covered_bytes = 0usize;
    let has_more = with_topology_snapshot(|| -> Result<bool, SystemError> {
        let candidates = collect_mount_candidates(view, cursor)?;
        for (index, candidate) in candidates.iter().enumerate() {
            if let Some(visible) = VisibleMount::resolve_in_snapshot(candidate, view)? {
                covered_bytes = covered_bytes.saturating_add(minimum_line_len(&visible, kind));
                selected.push(visible);
                if covered_bytes >= budget {
                    return Ok(index + 1 < candidates.len());
                }
            }
        }
        Ok(false)
    })?;
    let selected_count = selected.len();
    let last_selected_id = selected.last().map(|mount| mount.mount_id);
    let mut record = String::new();

    for (index, visible) in selected.into_iter().enumerate() {
        let min_line_len = minimum_line_len(&visible, kind);
        let entry = ProcMountEntry::from_visible(visible);
        record.clear();
        let fields = MountProcFields::from_entry(&entry)?;
        match kind {
            ProcMountRenderKind::Mounts => mounts_line::render(&fields, &mut record)?,
            ProcMountRenderKind::MountInfo => mountinfo_line::render(&fields, &mut record)?,
            ProcMountRenderKind::MountStats => mountstats_line::render(&fields, &mut record)?,
        }
        debug_assert!(record.len() >= min_line_len);
        out.extend_from_slice(record.as_bytes());
        if out.len() >= budget {
            // A record that crossed the bound is handed out with this slice;
            // only a slice that has nothing left to reach ends the record.
            return Ok(if index + 1 < selected_count || has_more {
                Some(entry.mount_id)
            } else {
                None
            });
        }
    }

    // If there are more candidates, the minimum line length proves the selected
    // records filled the budget. This arm is defensive if a formatter changes.
    debug_assert!(!has_more || out.len() >= budget);
    Ok(if has_more { last_selected_id } else { None })
}
