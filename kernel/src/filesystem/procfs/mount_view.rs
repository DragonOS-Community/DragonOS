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
        procfs::{utils::proc_read, ProcfsFilePrivateData},
        vfs::{mount::MountList, FilePrivateData, IndexNode, MountFS},
    },
    libs::mutex::MutexGuard,
    process::{ProcessControlBlock, ProcessManager, RawPid},
};

#[derive(Clone, Copy, Debug)]
pub(crate) enum ProcMountRenderKind {
    Mounts,
    MountInfo,
    MountStats,
}

#[derive(Debug)]
struct ProcMountEntry {
    mount: Arc<MountFS>,
    mountpoint_display: String,
    parent_mount_id: usize,
}

pub(crate) fn open_current_mount_file(
    kind: ProcMountRenderKind,
    data: &mut FilePrivateData,
) -> Result<(), SystemError> {
    open_pid_mount_file(ProcessManager::current_pid(), kind, data)
}

pub(crate) fn open_pid_mount_file(
    pid: RawPid,
    kind: ProcMountRenderKind,
    data: &mut FilePrivateData,
) -> Result<(), SystemError> {
    let rendered = render_mount_file(pid, kind)?;
    *data = FilePrivateData::Procfs(ProcfsFilePrivateData { data: rendered });
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

fn render_mount_file(pid: RawPid, kind: ProcMountRenderKind) -> Result<Vec<u8>, SystemError> {
    let target = ProcessManager::find(pid).ok_or(SystemError::ESRCH)?;
    let entries = collect_visible_mounts(&target)?;
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

fn collect_visible_mounts(
    target: &Arc<ProcessControlBlock>,
) -> Result<Vec<ProcMountEntry>, SystemError> {
    let nsproxy = target.nsproxy();
    let mount_list = nsproxy.mnt_ns.mount_list();
    let root_path = target
        .fs_struct()
        .root()
        .absolute_path()
        .unwrap_or_else(|_| "/".to_string());
    let root_prefix_with_slash = root_prefix_with_slash(&root_path);

    let mut entries = Vec::new();
    collect_mounts_preorder(
        nsproxy.mnt_ns.root_mntfs().clone(),
        "/".to_string(),
        &mount_list,
        &root_path,
        &root_prefix_with_slash,
        &mut entries,
    )?;
    Ok(entries)
}

fn collect_mounts_preorder(
    mount: Arc<MountFS>,
    mountpoint: String,
    mount_list: &Arc<MountList>,
    root_path: &str,
    root_prefix_with_slash: &str,
    out: &mut Vec<ProcMountEntry>,
) -> Result<(), SystemError> {
    if let Some(display) = visible_mountpoint(&mountpoint, root_path, root_prefix_with_slash) {
        let parent_mount_id: usize = mount
            .parent_mount()
            .map(|parent| parent.mount_id().into())
            .unwrap_or_else(|| mount.mount_id().into());

        out.push(ProcMountEntry {
            mount: mount.clone(),
            mountpoint_display: display,
            parent_mount_id,
        });
    }

    let children: Vec<Arc<MountFS>> = mount.mountpoints().values().cloned().collect();
    for child in children {
        let Some(child_path) = mount_list.get_mount_path_by_mountfs(&child) else {
            continue;
        };

        collect_mounts_preorder(
            child,
            child_path.as_str().to_string(),
            mount_list,
            root_path,
            root_prefix_with_slash,
            out,
        )?;
    }

    Ok(())
}

fn render_mounts_line(entry: &ProcMountEntry, out: &mut String) -> Result<(), SystemError> {
    let devname = escape_mount_token(&render_devname(&entry.mount)?, true);
    let mountpoint = escape_path_token(&entry.mountpoint_display);
    let fstype = escape_mount_token(entry.mount.fs_type(), true);
    let options = render_mount_options(&entry.mount)?;

    writeln!(out, "{devname} {mountpoint} {fstype} {options} 0 0").map_err(|_| SystemError::EINVAL)
}

fn render_mountinfo_line(entry: &ProcMountEntry, out: &mut String) -> Result<(), SystemError> {
    let mount_id: usize = entry.mount.mount_id().into();
    let dev = DeviceNumber::from(entry.mount.mountpoint_root_inode().metadata()?.dev_id as u32);
    let root = escape_path_token(&render_mountinfo_root(&entry.mount)?);
    let mountpoint = escape_path_token(&entry.mountpoint_display);
    let mount_options = render_mount_options(&entry.mount)?;
    let tags = render_mountinfo_tags(&entry.mount);
    let fstype = escape_mount_token(entry.mount.fs_type(), true);
    let source = escape_mount_token(&render_devname(&entry.mount)?, true);
    let super_options = render_mount_options(&entry.mount)?;

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
    let devname = escape_mount_token(&render_devname(&entry.mount)?, true);
    let mountpoint = escape_path_token(&entry.mountpoint_display);
    let fstype = escape_mount_token(entry.mount.fs_type(), true);
    let mut stats = String::new();
    let has_stats = entry
        .mount
        .inner_filesystem()
        .proc_show_mount_stats(&entry.mount, &mut stats)?;

    write!(
        out,
        "device {devname} mounted on {mountpoint} with fstype {fstype}"
    )
    .map_err(|_| SystemError::EINVAL)?;
    if has_stats && !stats.is_empty() {
        write!(out, " {stats}").map_err(|_| SystemError::EINVAL)?;
    }
    out.write_char('\n').map_err(|_| SystemError::EINVAL)
}

fn render_devname(mount: &Arc<MountFS>) -> Result<String, SystemError> {
    let mut devname = String::new();
    mount
        .inner_filesystem()
        .proc_show_devname(mount, &mut devname)?;
    Ok(devname)
}

fn render_mount_options(mount: &Arc<MountFS>) -> Result<String, SystemError> {
    let mut options = mount.mount_flags().proc_mount_options_string();
    let mut extra = String::new();
    mount
        .inner_filesystem()
        .proc_show_mount_options(mount, &mut extra)?;

    if !extra.is_empty() {
        if !options.is_empty() {
            options.push(',');
        }
        options.push_str(&extra);
    }

    Ok(options)
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
    if propagation.is_shared() {
        return format!("shared:{}", propagation.peer_group_id().data());
    }

    if propagation.is_slave() {
        if let Some(master) = propagation.master() {
            let master_group_id = master.propagation().peer_group_id();
            if master_group_id.is_valid() {
                return format!("master:{}", master_group_id.data());
            }
        }

        let peer_group_id = propagation.peer_group_id();
        if peer_group_id.is_valid() {
            return format!("master:{}", peer_group_id.data());
        }
    }

    if propagation.is_unbindable() {
        return "unbindable".to_string();
    }

    String::new()
}

fn visible_mountpoint(
    mountpoint: &str,
    root_path: &str,
    root_prefix_with_slash: &str,
) -> Option<String> {
    if root_path == "/" {
        return Some(mountpoint.to_string());
    }

    if mountpoint == root_path {
        return Some("/".to_string());
    }

    if mountpoint.starts_with(root_prefix_with_slash) {
        let stripped = &mountpoint[root_path.len()..];
        return Some(if stripped.is_empty() {
            "/".to_string()
        } else {
            stripped.to_string()
        });
    }

    None
}

fn root_prefix_with_slash(root_path: &str) -> String {
    if root_path == "/" || root_path.ends_with('/') {
        root_path.to_string()
    } else {
        format!("{root_path}/")
    }
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
