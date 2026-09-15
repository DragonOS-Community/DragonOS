use alloc::{string::String, sync::Arc, vec::Vec};

use system_error::SystemError;

use crate::{
    filesystem::vfs::mount::with_topology_snapshot,
    process::ProcessControlBlock,
};

use super::{
    collect::collect_visible_mounts,
    fields::MountProcFields,
    format::{mountinfo_line, mounts_line, mountstats_line},
};

#[derive(Clone, Copy, Debug)]
pub(crate) enum ProcMountRenderKind {
    Mounts,
    MountInfo,
    MountStats,
}

/// Render one mount-family record (`mounts` / `mountinfo` / `mountstats`) for
/// `target`.
///
/// Linux serves these through `seq_open_private()` (`fs/proc_namespace.c`): the
/// record is produced by the reader, not at open time, so a file that is opened
/// and read much later shows the topology of the moment it is read.
pub(crate) fn render_mount_file_for_task(
    target: &Arc<ProcessControlBlock>,
    kind: ProcMountRenderKind,
) -> Result<Vec<u8>, SystemError> {
    let (entries, _root_path) = with_topology_snapshot(|| collect_visible_mounts(target))?;
    let mut rendered = String::new();

    for entry in &entries {
        let fields = MountProcFields::from_entry(entry)?;
        match kind {
            ProcMountRenderKind::Mounts => mounts_line::render(&fields, &mut rendered)?,
            ProcMountRenderKind::MountInfo => mountinfo_line::render(&fields, &mut rendered)?,
            ProcMountRenderKind::MountStats => mountstats_line::render(&fields, &mut rendered)?,
        }
    }

    Ok(rendered.into_bytes())
}
