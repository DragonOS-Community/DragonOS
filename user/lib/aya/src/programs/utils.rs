use std::{
    fs::File,
    io,
    io::{BufRead, BufReader},
    os::fd::{AsRawFd, BorrowedFd},
    path::Path,
};

use crate::programs::ProgramError;

/// Get the specified information from a file descriptor's fdinfo.
pub(crate) fn get_fdinfo(fd: BorrowedFd<'_>, key: &str) -> Result<u32, ProgramError> {
    let info = File::open(format!("/proc/self/fdinfo/{}", fd.as_raw_fd()))?;
    let reader = BufReader::new(info);
    for line in reader.lines() {
        let line = line?;
        if !line.contains(key) {
            continue;
        }

        let (_key, val) = line.rsplit_once('\t').unwrap();

        return Ok(val.parse().unwrap());
    }
    Ok(0)
}

/// Find tracefs filesystem path.
pub(crate) fn find_tracefs_path() -> Result<&'static Path, ProgramError> {
    lazy_static::lazy_static! {
        static ref TRACE_FS: Option<&'static Path> = {
            let known_mounts = [
                Path::new("/sys/kernel/tracing"),
                Path::new("/sys/kernel/debug/tracing"),
            ];

            for mount in known_mounts {
                // Check that the mount point exists and is not empty
                // Documented here: (https://www.kernel.org/doc/Documentation/trace/ftrace.txt)
                // In some cases, tracefs will only mount at /sys/kernel/debug/tracing
                // but, the kernel will still create the directory /sys/kernel/tracing.
                // The user may be expected to manually mount the directory in order for it to
                // exist in /sys/kernel/tracing according to the documentation.
                if mount.exists() && mount.read_dir().ok()?.next().is_some() {
                    return Some(mount);
                }
            }
            None
        };
    }

    TRACE_FS
        .as_deref()
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "tracefs not found").into())
}
