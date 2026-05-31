//! Unified rendering for `/proc/mounts`, `/proc/[pid]/mounts`, `/proc/[pid]/mountinfo`,
//! and `/proc/[pid]/mountstats`.
//!
//! Content is generated once at `open()` and cached in `FilePrivateData` until the fd is closed.

use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::fmt::Write;

use system_error::SystemError;

use crate::{
    driver::base::device::device_number::DeviceNumber,
    filesystem::{
        procfs::{pid::ProcPidTarget, utils::proc_read},
        vfs::{mount::MountFlags, FilePrivateData, IndexNode, MountFS},
    },
    libs::mutex::MutexGuard,
    process::{namespace::mnt::MntNamespace, ProcessControlBlock, ProcessManager},
};

#[derive(Clone, Copy, Debug)]
pub(crate) enum ProcMountRenderKind {
    Mounts,
    MountInfo,
    MountStats,
}

struct ProcMountView {
    mntns: Arc<MntNamespace>,
    root_path: String,
}

#[derive(Debug)]
struct ProcMountEntry {
    mount: Arc<MountFS>,
    mountpoint_display: String,
    parent_mount_id: usize,
}

pub(crate) fn open_current_mount_file(
    kind: ProcMountRenderKind,
    data: &mut MutexGuard<FilePrivateData>,
) -> Result<(), SystemError> {
    let current = ProcessManager::current_pcb();
    open_mount_file_for_task(&current, kind, data)
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
    let rendered = ProcMountView::from_task(task)?.render(kind)?;
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
        _ => Err(SystemError::EIO),
    }
}

impl ProcMountView {
    fn from_task(target: &Arc<ProcessControlBlock>) -> Result<Self, SystemError> {
        let nsproxy = target.nsproxy();
        let mntns = nsproxy.mnt_namespace().clone();
        let root_path = target.fs_struct().root().absolute_path()?;

        Ok(Self { mntns, root_path })
    }

    fn render(&self, kind: ProcMountRenderKind) -> Result<Vec<u8>, SystemError> {
        let entries = self.collect_visible_mounts();
        let mut rendered = String::new();

        for entry in &entries {
            match kind {
                ProcMountRenderKind::Mounts => render_mounts_line(entry, &mut rendered)?,
                ProcMountRenderKind::MountInfo => render_mountinfo_line(entry, &mut rendered)?,
                ProcMountRenderKind::MountStats => render_mountstats_line(entry, &mut rendered)?,
            }
        }

        Ok(rendered.into_bytes())
    }

    fn collect_visible_mounts(&self) -> Vec<ProcMountEntry> {
        let mut mounts = self
            .mntns
            .mount_list()
            .clone_records()
            .into_iter()
            .map(|(path, mfs)| (path.as_str().to_string(), mfs))
            .collect::<Vec<_>>();

        mounts.sort_by_key(|(_, mfs)| {
            let mount_id: usize = mfs.mount_id().into();
            mount_id
        });

        let mut entries = Vec::new();
        let mut seen_mount_ids = Vec::<usize>::new();
        for (mount_path, mount) in mounts {
            let mount_id: usize = mount.mount_id().into();
            let Some(mountpoint_display) = visible_mountpoint(&mount_path, &self.root_path) else {
                continue;
            };
            if seen_mount_ids.contains(&mount_id) {
                continue;
            }
            seen_mount_ids.push(mount_id);

            let parent_mount_id: usize = mount
                .self_mountpoint()
                .map(|mountpoint_inode| mountpoint_inode.mount_fs().mount_id().into())
                .unwrap_or_else(|| mount.mount_id().into());

            entries.push(ProcMountEntry {
                mount,
                mountpoint_display,
                parent_mount_id,
            });
        }

        entries
    }
}

fn render_mounts_line(entry: &ProcMountEntry, out: &mut String) -> Result<(), SystemError> {
    let devname = escape_mount_token(&render_devname_or_none(&entry.mount)?, true);
    let mountpoint = escape_path_token(&entry.mountpoint_display);
    let fstype = escape_mount_token(entry.mount.fs_type(), true);
    let options = render_proc_mount_options(&entry.mount)?;

    writeln!(out, "{devname} {mountpoint} {fstype} {options} 0 0").map_err(|_| SystemError::EINVAL)
}

fn render_mountinfo_line(entry: &ProcMountEntry, out: &mut String) -> Result<(), SystemError> {
    let mount_id: usize = entry.mount.mount_id().into();
    let dev = entry
        .mount
        .mountpoint_root_inode()
        .metadata()
        .map(|md| DeviceNumber::from(md.dev_id as u32))
        .unwrap_or_default();
    let root = escape_path_token(&render_mountinfo_root(&entry.mount)?);
    let mountpoint = escape_path_token(&entry.mountpoint_display);
    let mount_options = render_mountinfo_mount_options(&entry.mount);
    let tags = render_mountinfo_tags(&entry.mount);
    let fstype = escape_mount_token(entry.mount.fs_type(), true);
    let source = escape_mount_token(&render_devname_or_none(&entry.mount)?, true);
    let super_options = render_mountinfo_super_options(&entry.mount)?;

    write!(
        out,
        "{mount_id} {} {}:{} {root} {mountpoint} {mount_options}",
        entry.parent_mount_id,
        dev.major().data(),
        dev.minor(),
    )
    .map_err(|_| SystemError::EINVAL)?;

    if !tags.is_empty() {
        write!(out, " {tags}").map_err(|_| SystemError::EINVAL)?;
    }

    writeln!(out, " - {fstype} {source} {super_options}").map_err(|_| SystemError::EINVAL)
}

fn render_mountstats_line(entry: &ProcMountEntry, out: &mut String) -> Result<(), SystemError> {
    match render_devname(&entry.mount)? {
        Some(devname) => {
            let devname = escape_mount_token(&devname, true);
            write!(out, "device {devname}").map_err(|_| SystemError::EINVAL)?;
        }
        None => out
            .write_str("no device")
            .map_err(|_| SystemError::EINVAL)?,
    }

    let mountpoint = escape_path_token(&entry.mountpoint_display);
    let fstype = escape_mount_token(entry.mount.fs_type(), true);
    let mut stats = String::new();
    let has_stats = match entry
        .mount
        .inner_filesystem()
        .proc_show_mount_stats(&entry.mount, &mut stats)
    {
        Ok(value) => value,
        Err(err) => {
            log::warn!(
                "proc_show_mount_stats failed for {}: {:?}",
                entry.mountpoint_display,
                err
            );
            false
        }
    };

    write!(out, " mounted on {mountpoint} with fstype {fstype}")
        .map_err(|_| SystemError::EINVAL)?;
    if has_stats && !stats.is_empty() {
        write!(out, " {stats}").map_err(|_| SystemError::EINVAL)?;
    }
    out.write_char('\n').map_err(|_| SystemError::EINVAL)
}

fn render_devname(mount: &Arc<MountFS>) -> Result<Option<String>, SystemError> {
    mount.inner_filesystem().proc_show_devname(mount)
}

fn render_devname_or_none(mount: &Arc<MountFS>) -> Result<String, SystemError> {
    Ok(render_devname(mount)?.unwrap_or_else(|| "none".to_string()))
}

fn render_proc_mount_options(mount: &Arc<MountFS>) -> Result<String, SystemError> {
    let mut options = Vec::new();
    options.push(if mount.is_readonly() { "ro" } else { "rw" });
    append_super_options(mount.super_block_flags(), &mut options);
    append_mount_options(mount.mount_flags(), &mut options);

    let mut rendered = options.join(",");
    append_fs_options(mount, &mut rendered)?;
    Ok(rendered)
}

fn render_mountinfo_mount_options(mount: &Arc<MountFS>) -> String {
    let mut options = Vec::new();
    let flags = mount.mount_flags();
    options.push(if flags.contains(MountFlags::RDONLY) {
        "ro"
    } else {
        "rw"
    });
    append_mount_options(flags, &mut options);
    options.join(",")
}

fn render_mountinfo_super_options(mount: &Arc<MountFS>) -> Result<String, SystemError> {
    let mut options = Vec::new();
    let flags = mount.super_block_flags();
    options.push(if flags.contains(MountFlags::RDONLY) {
        "ro"
    } else {
        "rw"
    });
    append_super_options(flags, &mut options);

    let mut rendered = options.join(",");
    append_fs_options(mount, &mut rendered)?;
    Ok(rendered)
}

fn append_fs_options(mount: &Arc<MountFS>, out: &mut String) -> Result<(), SystemError> {
    let mut extra = String::new();
    mount
        .inner_filesystem()
        .proc_show_mount_options(mount, &mut extra)?;

    if !extra.is_empty() {
        if !out.is_empty() {
            out.push(',');
        }
        out.push_str(&extra);
    }

    Ok(())
}

fn append_mount_options(flags: MountFlags, options: &mut Vec<&str>) {
    if flags.contains(MountFlags::NOSUID) {
        options.push("nosuid");
    }
    if flags.contains(MountFlags::NODEV) {
        options.push("nodev");
    }
    if flags.contains(MountFlags::NOEXEC) {
        options.push("noexec");
    }
    if flags.contains(MountFlags::NOATIME) {
        options.push("noatime");
    }
    if flags.contains(MountFlags::NODIRATIME) {
        options.push("nodiratime");
    }
    if flags.contains(MountFlags::RELATIME) {
        options.push("relatime");
    }
    if flags.contains(MountFlags::NOSYMFOLLOW) {
        options.push("nosymfollow");
    }
}

fn append_super_options(flags: MountFlags, options: &mut Vec<&str>) {
    if flags.contains(MountFlags::SYNCHRONOUS) {
        options.push("sync");
    }
    if flags.contains(MountFlags::DIRSYNC) {
        options.push("dirsync");
    }
    if flags.contains(MountFlags::MANDLOCK) {
        options.push("mand");
    }
    if flags.contains(MountFlags::LAZYTIME) {
        options.push("lazytime");
    }
}

fn render_mountinfo_root(mount: &Arc<MountFS>) -> Result<String, SystemError> {
    let mut root = String::new();
    mount
        .inner_filesystem()
        .proc_show_mountinfo_root(mount, &mut root)?;
    Ok(root)
}

fn render_mountinfo_tags(mount: &Arc<MountFS>) -> String {
    let propagation = mount.propagation();
    let mut fields = Vec::new();
    let info = propagation.info_string();
    if !info.is_empty() {
        fields.push(info);
    }
    if propagation.is_unbindable() {
        fields.push("unbindable".to_string());
    }
    fields.join(" ")
}

fn visible_mountpoint(mountpoint: &str, root_path: &str) -> Option<String> {
    if root_path == "/" {
        return Some(mountpoint.to_string());
    }

    let root_prefix_with_slash = if root_path.ends_with('/') {
        root_path.to_string()
    } else {
        format!("{root_path}/")
    };

    if mountpoint == root_path {
        return Some("/".to_string());
    }

    if let Some(stripped) = mountpoint.strip_prefix(&root_prefix_with_slash) {
        return Some(if stripped.is_empty() {
            "/".to_string()
        } else {
            format!("/{stripped}")
        });
    }

    None
}

fn escape_mount_token(input: &str, escape_hash: bool) -> String {
    escape_proc_field(input, escape_hash)
}

fn escape_path_token(input: &str) -> String {
    escape_proc_field(input, false)
}

fn escape_proc_field(input: &str, escape_hash: bool) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            ' ' => escaped.push_str("\\040"),
            '\t' => escaped.push_str("\\011"),
            '\n' => escaped.push_str("\\012"),
            '\\' => escaped.push_str("\\134"),
            '#' if escape_hash => escaped.push_str("\\043"),
            _ => escaped.push(ch),
        }
    }
    escaped
}
