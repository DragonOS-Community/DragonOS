use alloc::{string::String, string::ToString, sync::Arc};

use system_error::SystemError;

use crate::{
    driver::base::device::device_number::DeviceNumber,
    filesystem::vfs::{mount::append_comma_options, IndexNode, MountFS},
};

use super::collect::ProcMountEntry;

pub(crate) struct MountProcFields {
    pub mount: Arc<MountFS>,
    pub mountpoint_display: String,
    pub parent_mount_id: usize,
    pub devname: String,
    pub fstype: String,
    pub per_mount_options: String,
    pub mounts_options: String,
    pub super_block_options: String,
    pub mountinfo_root: String,
    pub mountinfo_tags: String,
    pub mount_id: usize,
    pub dev: DeviceNumber,
}

impl MountProcFields {
    pub(crate) fn from_entry(entry: &ProcMountEntry) -> Result<Self, SystemError> {
        let mount = entry.mount.clone();
        let devname = render_devname(&mount)?;
        let mountinfo_root = render_mountinfo_root(&mount)?;
        let per_mount_options = build_per_mount_options(&mount)?;
        let mut mounts_options = per_mount_options.clone();
        append_fs_mount_options(&mount, &mut mounts_options)?;
        let super_block_options = build_super_block_options(&mount)?;
        let mountinfo_tags = mount.propagation().proc_mountinfo_tags();
        let dev = mount
            .mountpoint_root_inode()
            .metadata()
            .map(|md| DeviceNumber::from(md.dev_id as u32))
            .unwrap_or_default();

        Ok(Self {
            mount_id: mount.mount_id().into(),
            dev,
            mount,
            mountpoint_display: entry.mountpoint_display.clone(),
            parent_mount_id: entry.parent_mount_id,
            devname,
            fstype: entry.mount.fs_type().to_string(),
            per_mount_options,
            mounts_options,
            super_block_options,
            mountinfo_root,
            mountinfo_tags,
        })
    }
}

fn build_per_mount_options(mount: &MountFS) -> Result<String, SystemError> {
    let flags = mount.mount_flags();
    let mut options = flags.proc_rw_token().to_string();
    append_comma_options(&mut options, flags.proc_per_mount_options());
    Ok(options)
}

fn build_super_block_options(mount: &MountFS) -> Result<String, SystemError> {
    let sb = mount.super_block_flags();
    let mut options = if mount.is_sb_readonly() {
        "ro".to_string()
    } else {
        "rw".to_string()
    };
    append_comma_options(&mut options, sb.proc_super_block_options());
    append_fs_mount_options(mount, &mut options)?;
    Ok(options)
}

fn append_fs_mount_options(mount: &MountFS, options: &mut String) -> Result<(), SystemError> {
    let mut extra = String::new();
    mount
        .inner_filesystem()
        .proc_show_mount_options(mount, &mut extra)?;
    append_comma_options(options, extra);
    Ok(())
}

fn render_devname(mount: &Arc<MountFS>) -> Result<String, SystemError> {
    let mut devname = String::new();
    mount
        .inner_filesystem()
        .proc_show_devname(mount, &mut devname)?;
    Ok(devname)
}

fn render_mountinfo_root(mount: &Arc<MountFS>) -> Result<String, SystemError> {
    let mut root = String::new();
    mount
        .inner_filesystem()
        .proc_show_mountinfo_root(mount, &mut root)?;
    Ok(root)
}
