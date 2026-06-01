use alloc::string::String;
use core::fmt::Write;

use system_error::SystemError;

use super::super::{escape::escape_mount_token, escape::escape_path_token, fields::MountProcFields};

pub(crate) fn render(fields: &MountProcFields, out: &mut String) -> Result<(), SystemError> {
    let devname = escape_mount_token(&fields.devname, true);
    let mountpoint = escape_path_token(&fields.mountpoint_display);
    let fstype = escape_mount_token(&fields.fstype, true);
    let mut stats = String::new();
    let has_stats = match fields
        .mount
        .inner_filesystem()
        .proc_show_mount_stats(&fields.mount, &mut stats)
    {
        Ok(value) => value,
        Err(err) => {
            log::warn!(
                "proc_show_mount_stats failed for {}: {:?}",
                fields.mountpoint_display,
                err
            );
            false
        }
    };

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
