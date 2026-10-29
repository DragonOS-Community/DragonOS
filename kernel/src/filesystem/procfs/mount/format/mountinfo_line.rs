use alloc::string::String;
use core::fmt::Write;

use system_error::SystemError;

use super::super::{
    escape::escape_mount_token, escape::escape_path_token, fields::MountProcFields,
};

pub(crate) fn render(fields: &MountProcFields, out: &mut String) -> Result<(), SystemError> {
    let root = escape_path_token(&fields.mountinfo_root);
    let mountpoint = escape_path_token(&fields.mountpoint_display);
    let mount_options = &fields.per_mount_options;
    let fstype = escape_mount_token(&fields.fstype, true);
    let source = escape_mount_token(&fields.devname, true);
    let super_options = &fields.super_block_options;

    write!(
        out,
        "{} {} {}:{} {root} {mountpoint} {mount_options}",
        fields.mount_id,
        fields.parent_mount_id,
        fields.dev.major().data(),
        fields.dev.minor(),
    )
    .map_err(|_| SystemError::EINVAL)?;

    if !fields.mountinfo_tags.is_empty() {
        write!(out, " {}", fields.mountinfo_tags).map_err(|_| SystemError::EINVAL)?;
    }

    writeln!(out, " - {fstype} {source} {super_options}").map_err(|_| SystemError::EINVAL)
}
