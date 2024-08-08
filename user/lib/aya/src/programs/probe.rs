use std::{
    ffi::{OsStr, OsString},
    format, fs,
    fs::OpenOptions,
    io,
    io::Write,
    os::fd::{AsFd, OwnedFd},
    path::{Path, PathBuf},
    string::String,
};

use libc::pid_t;

use crate::{
    programs::{
        kprobe::KProbeError,
        links::Link,
        perf_attach::{perf_attach, PerfLinkInner},
        uprobe::UProbeError,
        utils::find_tracefs_path,
        ProgramData, ProgramError,
    },
    sys::{perf_event::perf_event_open_probe, SyscallError},
    util::KernelVersion,
};

/// Kind of probe program
#[derive(Debug, Copy, Clone)]
pub enum ProbeKind {
    /// Kernel probe
    KProbe,
    /// Kernel return probe
    KRetProbe,
    /// User space probe
    UProbe,
    /// User space return probe
    URetProbe,
}

impl ProbeKind {
    fn pmu(&self) -> &'static str {
        match *self {
            Self::KProbe | Self::KRetProbe => "kprobe",
            Self::UProbe | Self::URetProbe => "uprobe",
        }
    }
}

#[derive(Debug)]
pub(crate) struct ProbeEvent {
    kind: ProbeKind,
    event_alias: String,
}

pub(crate) fn attach<T: Link + From<PerfLinkInner>>(
    program_data: &mut ProgramData<T>,
    kind: ProbeKind,
    // NB: the meaning of this argument is different for kprobe/kretprobe and uprobe/uretprobe; in
    // the kprobe case it is the name of the function to attach to, in the uprobe case it is a path
    // to the binary or library.
    //
    // TODO: consider encoding the type and the argument in the [`ProbeKind`] enum instead of a
    // separate argument.
    fn_name: &OsStr,
    offset: u64,
    pid: Option<pid_t>,
) -> Result<T::Id, ProgramError> {
    // https://github.com/torvalds/linux/commit/e12f03d7031a977356e3d7b75a68c2185ff8d155
    // Use debugfs to create probe
    let prog_fd = program_data.fd()?;
    let prog_fd = prog_fd.as_fd();
    let link = if KernelVersion::current().unwrap() < KernelVersion::new(4, 17, 0) {
        // let (fd, event_alias) = create_as_trace_point(kind, fn_name, offset, pid)?;
        // perf_attach_debugfs(prog_fd, fd, ProbeEvent { kind, event_alias })
        unimplemented!("The kernel version is too old to support perf events for probes")
    } else {
        let fd = create_as_probe(kind, fn_name, offset, pid)?;
        perf_attach(prog_fd, fd)
    }?;
    program_data.links.insert(T::from(link))
}

fn create_as_probe(
    kind: ProbeKind,
    fn_name: &OsStr,
    offset: u64,
    pid: Option<pid_t>,
) -> Result<OwnedFd, ProgramError> {
    info!(
        "create_as_probe: kind: {:?}, fn_name: {:?}, offset: {}, pid: {:?}",
        kind, fn_name, offset, pid
    );
    use ProbeKind::*;

    let perf_ty = match kind {
        KProbe | KRetProbe => read_sys_fs_perf_type(kind.pmu())
            .map_err(|(filename, io_error)| KProbeError::FileError { filename, io_error })?,
        UProbe | URetProbe => read_sys_fs_perf_type(kind.pmu())
            .map_err(|(filename, io_error)| UProbeError::FileError { filename, io_error })?,
    };

    let ret_bit = match kind {
        KRetProbe => Some(
            read_sys_fs_perf_ret_probe(kind.pmu())
                .map_err(|(filename, io_error)| KProbeError::FileError { filename, io_error })?,
        ),
        URetProbe => Some(
            read_sys_fs_perf_ret_probe(kind.pmu())
                .map_err(|(filename, io_error)| UProbeError::FileError { filename, io_error })?,
        ),
        _ => None,
    };

    perf_event_open_probe(perf_ty, ret_bit, fn_name, offset, pid).map_err(|(_code, io_error)| {
        SyscallError {
            call: "perf_event_open",
            io_error,
        }
        .into()
    })
}

pub(crate) fn detach_debug_fs(event: ProbeEvent) -> Result<(), ProgramError> {
    use ProbeKind::*;

    let tracefs = find_tracefs_path()?;

    let ProbeEvent {
        kind,
        event_alias: _,
    } = &event;
    let kind = *kind;
    let result = delete_probe_event(tracefs, event);

    result.map_err(|(filename, io_error)| match kind {
        KProbe | KRetProbe => KProbeError::FileError { filename, io_error }.into(),
        UProbe | URetProbe => UProbeError::FileError { filename, io_error }.into(),
    })
}

fn delete_probe_event(tracefs: &Path, event: ProbeEvent) -> Result<(), (PathBuf, io::Error)> {
    use std::os::unix::ffi::OsStrExt as _;

    let ProbeEvent { kind, event_alias } = event;
    let events_file_name = tracefs.join(format!("{}_events", kind.pmu()));

    fs::read(&events_file_name)
        .and_then(|events| {
            let found = lines(&events).any(|line| {
                let mut line = line.as_bytes();
                // See [`create_probe_event`] and the documentation:
                //
                // https://docs.kernel.org/trace/kprobetrace.html
                //
                // https://docs.kernel.org/trace/uprobetracer.html
                loop {
                    match line.split_first() {
                        None => break false,
                        Some((b, rest)) => {
                            line = rest;
                            if *b == b'/' {
                                break line.starts_with(event_alias.as_bytes());
                            }
                        }
                    }
                }
            });

            if found {
                OpenOptions::new()
                    .append(true)
                    .open(&events_file_name)
                    .and_then(|mut events_file| {
                        let mut rm = OsString::new();
                        rm.push("-:");
                        rm.push(event_alias);
                        rm.push("\n");

                        events_file.write_all(rm.as_bytes())
                    })
            } else {
                Ok(())
            }
        })
        .map_err(|e| (events_file_name, e))
}

pub(crate) fn lines(bytes: &[u8]) -> impl Iterator<Item = &OsStr> {
    use std::os::unix::ffi::OsStrExt as _;

    bytes.as_ref().split(|b| b == &b'\n').map(|mut line| {
        while let [stripped @ .., c] = line {
            if c.is_ascii_whitespace() {
                line = stripped;
                continue;
            }
            break;
        }
        OsStr::from_bytes(line)
    })
}

fn read_sys_fs_perf_type(pmu: &str) -> Result<u32, (PathBuf, io::Error)> {
    // let file = format!("/sys/bus/event_source/devices/{}/type", pmu);
    // let res = unsafe { extern_read_sys_fs_perf_type(&file) }.unwrap();
    let res = 6;
    Ok(res)
    // fs::read_to_string(&file)
    //     .and_then(|perf_ty| {
    //         perf_ty
    //             .trim()
    //             .parse::<u32>()
    //             .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    //     })
    //     .map_err(|e| (file, e))
}

fn read_sys_fs_perf_ret_probe(pmu: &str) -> Result<u32, (PathBuf, io::Error)> {
    // let file = Path::new("/sys/bus/event_source/devices")
    //     .join(pmu)
    //     .join("format/retprobe");
    //
    // fs::read_to_string(&file)
    //     .and_then(|data| {
    //         let mut parts = data.trim().splitn(2, ':').skip(1);
    //         let config = parts
    //             .next()
    //             .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "invalid format"))?;
    //
    //         config
    //             .parse::<u32>()
    //             .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    //     })
    //     .map_err(|e| (file, e))
    // let file = format!("/sys/bus/event_source/devices/{}/format/retprobe", pmu);
    // let res = unsafe { extern_read_sys_fs_perf_ret_probe(&file) }.unwrap();
    Ok(0)
}
