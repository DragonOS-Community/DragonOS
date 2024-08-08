pub mod perf;

use core::mem;
use std::{
    ffi::CString,
    fmt, io,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    path::Path,
};

use aya_obj::{
    generated::bpf_map_info,
    maps,
    maps::{InvalidMapTypeError, PinningType},
    parse_map_info, EbpfSectionKind,
};
pub use perf::PerfEventArray;
use libc::{getrlimit, rlim_t, rlimit, RLIMIT_MEMLOCK, RLIM_INFINITY};
use thiserror::Error;

#[cfg(any(feature = "async_tokio", feature = "async_std"))]
use crate::maps::perf::AsyncPerfEventArray;
use crate::{
    pin::PinError,
    sys::{
        bpf_create_map, bpf_get_object, bpf_map_freeze, bpf_map_get_fd_by_id,
        bpf_map_get_info_by_fd, bpf_map_update_elem_ptr, bpf_pin_object, iter_map_ids,
        SyscallError,
    },
    util::{bytes_of_bpf_name, KernelVersion},
};

#[derive(Error, Debug)]
/// Errors occuring from working with Maps
pub enum MapError {
    /// Invalid map type encontered
    #[error("invalid map type {map_type}")]
    InvalidMapType {
        /// The map type
        map_type: u32,
    },

    /// Invalid map name encountered
    #[error("invalid map name `{name}`")]
    InvalidName {
        /// The map name
        name: String,
    },

    /// Failed to create map
    #[error("failed to create map `{name}` with code {code}")]
    CreateError {
        /// Map name
        name: String,
        /// Error code
        code: i64,
        #[source]
        /// Original io::Error
        io_error: io::Error,
    },

    /// Invalid key size
    #[error("invalid key size {size}, expected {expected}")]
    InvalidKeySize {
        /// Size encountered
        size: usize,
        /// Size expected
        expected: usize,
    },

    /// Invalid value size
    #[error("invalid value size {size}, expected {expected}")]
    InvalidValueSize {
        /// Size encountered
        size: usize,
        /// Size expected
        expected: usize,
    },

    /// Index is out of bounds
    #[error("the index is {index} but `max_entries` is {max_entries}")]
    OutOfBounds {
        /// Index accessed
        index: u32,
        /// Map size
        max_entries: u32,
    },

    /// Key not found
    #[error("key not found")]
    KeyNotFound,

    /// Element not found
    #[error("element not found")]
    ElementNotFound,

    /// Progam Not Loaded
    #[error("the program is not loaded")]
    ProgramNotLoaded,

    /// Syscall failed
    #[error(transparent)]
    SyscallError(#[from] SyscallError),

    /// Could not pin map
    #[error("map `{name:?}` requested pinning. pinning failed")]
    PinError {
        /// The map name
        name: Option<String>,
        /// The reason for the failure
        #[source]
        error: PinError,
    },

    /// Program IDs are not supported
    #[error("program ids are not supported by the current kernel")]
    ProgIdNotSupported,

    /// Unsupported Map type
    #[error("Unsupported map type found {map_type}")]
    Unsupported {
        /// The map type
        map_type: u32,
    },
}

// Note that this is not just derived using #[from] because InvalidMapTypeError cannot implement
// Error due the the fact that aya-obj is no_std and error_in_core is not stabilized
// (https://github.com/rust-lang/rust/issues/103765).
impl From<InvalidMapTypeError> for MapError {
    fn from(e: InvalidMapTypeError) -> Self {
        let InvalidMapTypeError { map_type } = e;
        Self::InvalidMapType { map_type }
    }
}

/// A map file descriptor.
#[derive(Debug)]
pub struct MapFd {
    fd: crate::MockableFd,
}

impl MapFd {
    fn from_fd(fd: OwnedFd) -> Self {
        let fd = crate::MockableFd::from_fd(fd);
        Self { fd }
    }

    fn try_clone(&self) -> io::Result<Self> {
        let Self { fd } = self;
        let fd = fd.try_clone()?;
        Ok(Self { fd })
    }
}

impl AsFd for MapFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        let Self { fd } = self;
        fd.as_fd()
    }
}

/// eBPF map types.
#[derive(Debug)]
pub enum Map {
    /// An [`Array`] map.
    Array(MapData),
    /// A [`BloomFilter`] map.
    BloomFilter(MapData),
    /// A [`CpuMap`] map.
    CpuMap(MapData),
    /// A [`DevMap`] map.
    DevMap(MapData),
    /// A [`DevMapHash`] map.
    DevMapHash(MapData),
    /// A [`HashMap`] map.
    HashMap(MapData),
    /// A [`LpmTrie`] map.
    LpmTrie(MapData),
    /// A [`HashMap`] map that uses a LRU eviction policy.
    LruHashMap(MapData),
    /// A [`PerCpuArray`] map.
    PerCpuArray(MapData),
    /// A [`PerCpuHashMap`] map.
    PerCpuHashMap(MapData),
    /// A [`PerCpuHashMap`] map that uses a LRU eviction policy.
    PerCpuLruHashMap(MapData),
    /// A [`PerfEventArray`] map.
    PerfEventArray(MapData),
    /// A [`ProgramArray`] map.
    ProgramArray(MapData),
    /// A [`Queue`] map.
    Queue(MapData),
    /// A [`RingBuf`] map.
    RingBuf(MapData),
    /// A [`SockHash`] map
    SockHash(MapData),
    /// A [`SockMap`] map.
    SockMap(MapData),
    /// A [`Stack`] map.
    Stack(MapData),
    /// A [`StackTraceMap`] map.
    StackTraceMap(MapData),
    /// An unsupported map type.
    Unsupported(MapData),
    /// A [`XskMap`] map.
    XskMap(MapData),
}
impl Map {
    /// Returns the low level map type.
    fn map_type(&self) -> u32 {
        match self {
            Self::Array(map) => map.obj.map_type(),
            Self::BloomFilter(map) => map.obj.map_type(),
            Self::CpuMap(map) => map.obj.map_type(),
            Self::DevMap(map) => map.obj.map_type(),
            Self::DevMapHash(map) => map.obj.map_type(),
            Self::HashMap(map) => map.obj.map_type(),
            Self::LpmTrie(map) => map.obj.map_type(),
            Self::LruHashMap(map) => map.obj.map_type(),
            Self::PerCpuArray(map) => map.obj.map_type(),
            Self::PerCpuHashMap(map) => map.obj.map_type(),
            Self::PerCpuLruHashMap(map) => map.obj.map_type(),
            Self::PerfEventArray(map) => map.obj.map_type(),
            Self::ProgramArray(map) => map.obj.map_type(),
            Self::Queue(map) => map.obj.map_type(),
            Self::RingBuf(map) => map.obj.map_type(),
            Self::SockHash(map) => map.obj.map_type(),
            Self::SockMap(map) => map.obj.map_type(),
            Self::Stack(map) => map.obj.map_type(),
            Self::StackTraceMap(map) => map.obj.map_type(),
            Self::Unsupported(map) => map.obj.map_type(),
            Self::XskMap(map) => map.obj.map_type(),
        }
    }
    /// Pins the map to a BPF filesystem.
    ///
    /// When a map is pinned it will remain loaded until the corresponding file
    /// is deleted. All parent directories in the given `path` must already exist.
    pub fn pin<P: AsRef<Path>>(&self, path: P) -> Result<(), PinError> {
        match self {
            Self::Array(map) => map.pin(path),
            Self::BloomFilter(map) => map.pin(path),
            Self::CpuMap(map) => map.pin(path),
            Self::DevMap(map) => map.pin(path),
            Self::DevMapHash(map) => map.pin(path),
            Self::HashMap(map) => map.pin(path),
            Self::LpmTrie(map) => map.pin(path),
            Self::LruHashMap(map) => map.pin(path),
            Self::PerCpuArray(map) => map.pin(path),
            Self::PerCpuHashMap(map) => map.pin(path),
            Self::PerCpuLruHashMap(map) => map.pin(path),
            Self::PerfEventArray(map) => map.pin(path),
            Self::ProgramArray(map) => map.pin(path),
            Self::Queue(map) => map.pin(path),
            Self::RingBuf(map) => map.pin(path),
            Self::SockHash(map) => map.pin(path),
            Self::SockMap(map) => map.pin(path),
            Self::Stack(map) => map.pin(path),
            Self::StackTraceMap(map) => map.pin(path),
            Self::Unsupported(map) => map.pin(path),
            Self::XskMap(map) => map.pin(path),
        }
    }
}
/// A generic handle to a BPF map.
///
/// You should never need to use this unless you're implementing a new map type.
#[derive(Debug)]
pub struct MapData {
    obj: maps::Map,
    fd: MapFd,
}

impl MapData {
    /// Creates a new map with the provided `name`
    pub fn create(
        obj: maps::Map,
        name: &str,
        btf_fd: Option<BorrowedFd<'_>>,
    ) -> Result<Self, MapError> {
        let c_name = CString::new(name).map_err(|_| MapError::InvalidName { name: name.into() })?;

        // #[cfg(not(test))]
        // let kernel_version = KernelVersion::current().unwrap();
        // #[cfg(test)]
        let kernel_version = KernelVersion::new(0xff, 0xff, 0xff);
        let fd =
            bpf_create_map(&c_name, &obj, btf_fd, kernel_version).map_err(|(code, io_error)| {
                if kernel_version < KernelVersion::new(5, 11, 0) {
                    maybe_warn_rlimit();
                }

                MapError::CreateError {
                    name: name.into(),
                    code,
                    io_error,
                }
            })?;
        log::info!("created map with fd: {:?}", fd);
        Ok(Self {
            obj,
            fd: MapFd::from_fd(fd),
        })
    }

    pub(crate) fn finalize(&mut self) -> Result<(), MapError> {
        let Self { obj, fd } = self;
        if !obj.data().is_empty() && obj.section_kind() != EbpfSectionKind::Bss {
            log::error!(
                "map data is not empty, but section kind is not BSS, {:?}",
                obj.section_kind()
            );
            let data = obj.data();
            let value = u64::from_le_bytes(data[0..8].try_into().unwrap());
            log::error!(
                "bpf_map_update_elem_ptr, key_ptr: {:?}, value_ptr: {:?}, value: {}",
                &0 as *const _,
                obj.data_mut().as_mut_ptr(),
                value
            );
            bpf_map_update_elem_ptr(fd.as_fd(), &0 as *const _, obj.data_mut().as_mut_ptr(), 0)
                .map_err(|(_, io_error)| SyscallError {
                    call: "bpf_map_update_elem",
                    io_error,
                })
                .map_err(MapError::from)?;
        }
        if obj.section_kind() == EbpfSectionKind::Rodata {
            bpf_map_freeze(fd.as_fd())
                .map_err(|(_, io_error)| SyscallError {
                    call: "bpf_map_freeze",
                    io_error,
                })
                .map_err(MapError::from)?;
        }
        Ok(())
    }
    /// Allows the map to be pinned to the provided path.
    ///
    /// Any directories in the the path provided should have been created by the caller.
    /// The path must be on a BPF filesystem.
    ///
    /// # Errors
    ///
    /// Returns a [`PinError::SyscallError`] if the underlying syscall fails.
    /// This may also happen if the path already exists, in which case the wrapped
    /// [`std::io::Error`] kind will be [`std::io::ErrorKind::AlreadyExists`].
    /// Returns a [`PinError::InvalidPinPath`] if the path provided cannot be
    /// converted to a [`CString`].
    ///
    /// # Example
    ///
    /// ```no_run
    /// # let mut bpf = aya::Ebpf::load(&[])?;
    /// # use aya::maps::MapData;
    ///
    /// let mut map = MapData::from_pin("/sys/fs/bpf/my_map")?;
    /// map.pin("/sys/fs/bpf/my_map2")?;
    ///
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn pin<P: AsRef<Path>>(&self, path: P) -> Result<(), PinError> {
        use std::os::unix::ffi::OsStrExt as _;

        let Self { fd, obj: _ } = self;
        let path = path.as_ref();
        let path_string = CString::new(path.as_os_str().as_bytes()).map_err(|error| {
            PinError::InvalidPinPath {
                path: path.to_path_buf(),
                error,
            }
        })?;
        bpf_pin_object(fd.as_fd(), &path_string).map_err(|(_, io_error)| SyscallError {
            call: "BPF_OBJ_PIN",
            io_error,
        })?;
        Ok(())
    }
    pub(crate) fn obj(&self) -> &maps::Map {
        let Self { obj, fd: _ } = self;
        obj
    }

    pub fn from_id(id: u32) -> Result<Self, MapError> {
        let fd = bpf_map_get_fd_by_id(id)?;
        Self::from_fd(fd)
    }
    /// Loads a map from a file descriptor.
    ///
    /// If loading from a BPF Filesystem (bpffs) you should use [`Map::from_pin`](crate::maps::MapData::from_pin).
    /// This API is intended for cases where you have received a valid BPF FD from some other means.
    /// For example, you received an FD over Unix Domain Socket.
    pub fn from_fd(fd: OwnedFd) -> Result<Self, MapError> {
        let MapInfo(info) = MapInfo::new_from_fd(fd.as_fd())?;
        Ok(Self {
            obj: parse_map_info(info, PinningType::None),
            fd: MapFd::from_fd(fd),
        })
    }

    /// Returns the file descriptor of the map.
    pub fn fd(&self) -> &MapFd {
        let Self { obj: _, fd } = self;
        fd
    }
}

/// Raises a warning about rlimit. Should be used only if creating a map was not
/// successful.
fn maybe_warn_rlimit() {
    let mut limit = mem::MaybeUninit::<rlimit>::uninit();
    let ret = unsafe { getrlimit(RLIMIT_MEMLOCK, limit.as_mut_ptr()) };
    if ret == 0 {
        let limit = unsafe { limit.assume_init() };

        if limit.rlim_cur == RLIM_INFINITY {
            return;
        }
        struct HumanSize(rlim_t);

        impl fmt::Display for HumanSize {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let &Self(size) = self;
                if size < 1024 {
                    write!(f, "{} bytes", size)
                } else if size < 1024 * 1024 {
                    write!(f, "{} KiB", size / 1024)
                } else {
                    write!(f, "{} MiB", size / 1024 / 1024)
                }
            }
        }
        warn!(
            "RLIMIT_MEMLOCK value is {}, not RLIM_INFINITY; if experiencing problems with creating \
            maps, try raising RLIMIT_MEMLOCK either to RLIM_INFINITY or to a higher value sufficient \
            for the size of your maps",
            HumanSize(limit.rlim_cur)
        );
    }
}

/// Provides information about a loaded map, like name, id and size.
#[derive(Debug)]
pub struct MapInfo(bpf_map_info);

impl MapInfo {
    fn new_from_fd(fd: BorrowedFd<'_>) -> Result<Self, MapError> {
        let info = bpf_map_get_info_by_fd(fd.as_fd())?;
        Ok(Self(info))
    }

    /// Loads map info from a map id.
    pub fn from_id(id: u32) -> Result<Self, MapError> {
        bpf_map_get_fd_by_id(id)
            .map_err(MapError::from)
            .and_then(|fd| Self::new_from_fd(fd.as_fd()))
    }

    /// The name of the map, limited to 16 bytes.
    pub fn name(&self) -> &[u8] {
        bytes_of_bpf_name(&self.0.name)
    }

    /// The name of the map as a &str. If the name is not valid unicode, None is returned.
    pub fn name_as_str(&self) -> Option<&str> {
        std::str::from_utf8(self.name()).ok()
    }

    /// The id for this map. Each map has a unique id.
    pub fn id(&self) -> u32 {
        self.0.id
    }

    /// The map type as defined by the linux kernel enum
    /// [`bpf_map_type`](https://elixir.bootlin.com/linux/v6.4.4/source/include/uapi/linux/bpf.h#L905).
    pub fn map_type(&self) -> u32 {
        self.0.type_
    }

    /// The key size for this map.
    pub fn key_size(&self) -> u32 {
        self.0.key_size
    }

    /// The value size for this map.
    pub fn value_size(&self) -> u32 {
        self.0.value_size
    }

    /// The maximum number of entries in this map.
    pub fn max_entries(&self) -> u32 {
        self.0.max_entries
    }

    /// The flags for this map.
    pub fn map_flags(&self) -> u32 {
        self.0.map_flags
    }

    /// Returns a file descriptor referencing the map.
    ///
    /// The returned file descriptor can be closed at any time and doing so does
    /// not influence the life cycle of the map.
    pub fn fd(&self) -> Result<MapFd, MapError> {
        let Self(info) = self;
        let fd = bpf_map_get_fd_by_id(info.id)?;
        Ok(MapFd::from_fd(fd))
    }

    /// Loads a map from a pinned path in bpffs.
    pub fn from_pin<P: AsRef<Path>>(path: P) -> Result<Self, MapError> {
        use std::os::unix::ffi::OsStrExt as _;

        // TODO: avoid this unwrap by adding a new error variant.
        let path_string = CString::new(path.as_ref().as_os_str().as_bytes()).unwrap();
        let fd = bpf_get_object(&path_string).map_err(|(_, io_error)| SyscallError {
            call: "BPF_OBJ_GET",
            io_error,
        })?;

        Self::new_from_fd(fd.as_fd())
    }
}

/// Returns an iterator over all loaded bpf maps.
///
/// This differs from [`crate::Ebpf::maps`] since it will return all maps
/// listed on the host system and not only maps for a specific [`crate::Ebpf`] instance.
///
/// # Example
/// ```
/// # use aya::maps::loaded_maps;
///
/// for m in loaded_maps() {
///     match m {
///         Ok(map) => println!("{:?}", map.name_as_str()),
///         Err(e) => println!("Error iterating maps: {:?}", e),
///     }
/// }
/// ```
///
/// # Errors
///
/// Returns [`MapError::SyscallError`] if any of the syscalls required to either get
/// next map id, get the map fd, or the [`MapInfo`] fail. In cases where
/// iteration can't be performed, for example the caller does not have the necessary privileges,
/// a single item will be yielded containing the error that occurred.
pub fn loaded_maps() -> impl Iterator<Item = Result<MapInfo, MapError>> {
    iter_map_ids().map(|id| {
        let id = id?;
        MapInfo::from_id(id)
    })
}

// Implements TryFrom<Map> for different map implementations. Different map implementations can be
// constructed from different variants of the map enum. Also, the implementation may have type
// parameters (which we assume all have the bound `Pod` and nothing else).
macro_rules! impl_try_from_map {
    // At the root the type parameters are marked as a single token tree which will be pasted into
    // the invocation for each type. Note that the later patterns require that the token tree be
    // zero or more comma separated idents wrapped in parens. Note that the tt metavar is used here
    // rather than the repeated idents used later because the macro language does not allow one
    // repetition to be pasted inside another.
    ($ty_param:tt {
        $($ty:ident $(from $($variant:ident)|+)?),+ $(,)?
    }) => {
        $(impl_try_from_map!(<$ty_param> $ty $(from $($variant)|+)?);)+
    };
    // Add the "from $variant" using $ty as the default if it is missing.
    (<$ty_param:tt> $ty:ident) => {
        impl_try_from_map!(<$ty_param> $ty from $ty);
    };
    // Dispatch for each of the lifetimes.
    (
        <($($ty_param:ident),*)> $ty:ident from $($variant:ident)|+
    ) => {
        impl_try_from_map!(<'a> ($($ty_param),*) $ty from $($variant)|+);
        impl_try_from_map!(<'a mut> ($($ty_param),*) $ty from $($variant)|+);
        impl_try_from_map!(<> ($($ty_param),*) $ty from $($variant)|+);
    };
    // An individual impl.
    (
        <$($l:lifetime $($m:ident)?)?>
        ($($ty_param:ident),*)
        $ty:ident from $($variant:ident)|+
    ) => {
        impl<$($l,)? $($ty_param: Pod),*> TryFrom<$(&$l $($m)?)? Map>
            for $ty<$(&$l $($m)?)? MapData, $($ty_param),*>
        {
            type Error = MapError;

            fn try_from(map: $(&$l $($m)?)? Map) -> Result<Self, Self::Error> {
                match map {
                    $(Map::$variant(map_data) => Self::new(map_data),)+
                    map => Err(MapError::InvalidMapType {
                        map_type: map.map_type()
                    }),
                }
            }
        }
    };
}

#[cfg(any(feature = "async_tokio", feature = "async_std"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "async_tokio", feature = "async_std"))))]
impl_try_from_map!(() {
    AsyncPerfEventArray from PerfEventArray,
});

impl_try_from_map!(() {
    PerfEventArray,
});
