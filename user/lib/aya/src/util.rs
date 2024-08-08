use core::{
    slice,
    str::{FromStr, Utf8Error},
};
use std::io;

/// Represents a kernel version, in major.minor.release version.
// Adapted from https://docs.rs/procfs/latest/procfs/sys/kernel/struct.Version.html.
#[derive(Debug, Copy, Clone, Eq, PartialEq, PartialOrd)]
pub struct KernelVersion {
    pub(crate) major: u8,
    pub(crate) minor: u8,
    pub(crate) patch: u16,
}

#[derive(thiserror::Error, Debug)]
enum CurrentKernelVersionError {
    #[error("failed to read kernel version")]
    IO(#[from] io::Error),
    #[error("failed to parse kernel version")]
    ParseError(String),
    #[error("kernel version string is not valid UTF-8")]
    Utf8(#[from] Utf8Error),
}

impl KernelVersion {
    /// Constructor.
    pub fn new(major: u8, minor: u8, patch: u16) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
    /// Returns the kernel version of the currently running kernel.
    pub fn current() -> Result<Self, &'static str> {
        // Self::get_kernel_version()
        Ok(Self::new(0xff, 0xff, 0xff))
    }

    pub fn code(self) -> u32 {
        let Self {
            major,
            minor,
            mut patch,
        } = self;

        // Certain LTS kernels went above the "max" 255 patch so
        // backports were done to cap the patch version
        let max_patch = match (major, minor) {
            // On 4.4 + 4.9, any patch 257 or above was hardcoded to 255.
            // See: https://github.com/torvalds/linux/commit/a15813a +
            // https://github.com/torvalds/linux/commit/42efb098
            (4, 4 | 9) => 257,
            // On 4.14, any patch 252 or above was hardcoded to 255.
            // See: https://github.com/torvalds/linux/commit/e131e0e
            (4, 14) => 252,
            // On 4.19, any patch 222 or above was hardcoded to 255.
            // See: https://github.com/torvalds/linux/commit/a256aac
            (4, 19) => 222,
            // For other kernels (i.e., newer LTS kernels as other
            // ones won't reach 255+ patches) clamp it to 255. See:
            // https://github.com/torvalds/linux/commit/9b82f13e
            _ => 255,
        };

        // anything greater or equal to `max_patch` is hardcoded to
        // 255.
        if patch >= max_patch {
            patch = 255;
        }

        (u32::from(major) << 16) + (u32::from(minor) << 8) + u32::from(patch)
    }
}

/// Include bytes from a file for use in a subsequent [`crate::Ebpf::load`].
///
/// This macro differs from the standard `include_bytes!` macro since it also ensures that
/// the bytes are correctly aligned to be parsed as an ELF binary. This avoid some nasty
/// compilation errors when the resulting byte array is not the correct alignment.
///
/// # Examples
/// ```ignore
/// use aya::{Ebpf, include_bytes_aligned};
///
/// let mut bpf = Ebpf::load(include_bytes_aligned!(
///     "/path/to/bpf.o"
/// ))?;
///
/// # Ok::<(), aya::EbpfError>(())
/// ```
#[macro_export]
macro_rules! include_bytes_aligned {
    ($path:expr) => {{
        #[repr(align(32))]
        pub struct Aligned32;

        #[repr(C)]
        pub struct Aligned<Bytes: ?Sized> {
            pub _align: [Aligned32; 0],
            pub bytes: Bytes,
        }

        const ALIGNED: &Aligned<[u8]> = &Aligned {
            _align: [],
            bytes: *include_bytes!($path),
        };

        &ALIGNED.bytes
    }};
}

pub(crate) fn bytes_of_bpf_name(bpf_name: &[core::ffi::c_char; 16]) -> &[u8] {
    let length = bpf_name
        .iter()
        .rposition(|ch| *ch != 0)
        .map(|pos| pos + 1)
        .unwrap_or(0);
    unsafe { slice::from_raw_parts(bpf_name.as_ptr() as *const _, length) }
}

const ONLINE_CPUS: &str = "/sys/devices/system/cpu/online";
pub(crate) const POSSIBLE_CPUS: &str = "/sys/devices/system/cpu/possible";

/// Get the list of possible cpus.
///
/// See `/sys/devices/system/cpu/possible`.
pub(crate) fn possible_cpus() -> Result<Vec<u32>, io::Error> {
    // let data = fs::read_to_string(POSSIBLE_CPUS)?;
    // parse_cpu_ranges(data.trim()).map_err(|_| {
    //     io::Error::new(
    //         io::ErrorKind::Other,
    //         format!("unexpected {POSSIBLE_CPUS} format"),
    //     )
    // })
    Ok(vec![0])
}

fn parse_cpu_ranges(data: &str) -> Result<Vec<u32>, ()> {
    let mut cpus = Vec::new();
    for range in data.split(',') {
        cpus.extend({
            match range
                .splitn(2, '-')
                .map(u32::from_str)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| ())?
                .as_slice()
            {
                &[] | &[_, _, _, ..] => return Err(()),
                &[start] => start..=start,
                &[start, end] => start..=end,
            }
        })
    }

    Ok(cpus)
}

/// Returns the numeric IDs of the CPUs currently online.
pub fn online_cpus() -> Result<Vec<u32>, io::Error> {
    // let data = fs::read_to_string(ONLINE_CPUS)?;
    // parse_cpu_ranges(data.trim()).map_err(|_| {
    //     io::Error::new(
    //         io::ErrorKind::Other,
    //         format!("unexpected {ONLINE_CPUS} format"),
    //     )
    // })
    Ok(vec![0])
}

pub(crate) fn page_size() -> usize {
    // Safety: libc
    // (unsafe { sysconf(_SC_PAGESIZE) }) as usize
    4096
}
