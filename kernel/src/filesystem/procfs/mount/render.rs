use alloc::{string::String, sync::Arc, vec::Vec};

use system_error::SystemError;

use crate::{
    filesystem::{
        procfs::{pid::ProcPidTarget, utils::proc_read},
        vfs::FilePrivateData,
    },
    libs::mutex::MutexGuard,
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

pub(crate) fn open_mount_file_for_target(
    target: &ProcPidTarget,
    kind: ProcMountRenderKind,
    data: &mut MutexGuard<FilePrivateData>,
) -> Result<(), SystemError> {
    let task = target.thread_group_leader().ok_or(SystemError::ESRCH)?;
    open_mount_file_for_task(&task, kind, data)
}

fn open_mount_file_for_task(
    task: &Arc<ProcessControlBlock>,
    kind: ProcMountRenderKind,
    data: &mut MutexGuard<FilePrivateData>,
) -> Result<(), SystemError> {
    let rendered = render_mount_file_for_task(task, kind)?;
    let FilePrivateData::Procfs(pdata) = &mut **data else {
        return Err(SystemError::EIO);
    };
    pdata.data = rendered;
    Ok(())
}

pub(crate) fn read_cached_mount_file(
    offset: usize,
    len: usize,
    buf: &mut [u8],
    data: MutexGuard<FilePrivateData>,
) -> Result<usize, SystemError> {
    match &*data {
        FilePrivateData::Procfs(pdata) => proc_read(offset, len, buf, &pdata.data),
        _ => Err(SystemError::EINVAL),
    }
}

fn render_mount_file_for_task(
    target: &Arc<ProcessControlBlock>,
    kind: ProcMountRenderKind,
) -> Result<Vec<u8>, SystemError> {
    let (entries, _root_path) = collect_visible_mounts(target)?;
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
