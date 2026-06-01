use alloc::string::String;
use core::fmt::Write;

use system_error::SystemError;

use super::super::{
    escape::escape_mount_token, escape::escape_path_token, fields::MountProcFields,
};

pub(crate) fn render(fields: &MountProcFields, out: &mut String) -> Result<(), SystemError> {
    let devname = escape_mount_token(&fields.devname, true);
    let mountpoint = escape_path_token(&fields.mountpoint_display);
    let fstype = escape_mount_token(&fields.fstype, true);
    let options = &fields.mounts_options;

    writeln!(out, "{devname} {mountpoint} {fstype} {options} 0 0").map_err(|_| SystemError::EINVAL)
}
