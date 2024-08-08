pub mod extension;
pub mod kprobe;
pub mod links;
mod perf_attach;
pub mod probe;
pub mod uprobe;
mod utils;

use core::num::NonZeroU32;
use std::{
    ffi::CString,
    io,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    path::{Path, PathBuf},
    string::String,
    sync::Arc,
    vec,
    vec::Vec,
};

use aya_obj::{
    btf::BtfError,
    generated::{bpf_attach_type, bpf_prog_info, bpf_prog_type},
    obj, VerifierLog,
};
use thiserror::Error;

pub use crate::programs::{
    extension::{Extension, ExtensionError},
    kprobe::{KProbe, KProbeError},
    uprobe::{UProbe, UProbeError},
};
use crate::{
    maps::MapError,
    pin::PinError,
    programs::{
        links::{Link, LinkMap},
        utils::get_fdinfo,
    },
    sys::{
        bpf_btf_get_fd_by_id, bpf_get_object, bpf_load_program, bpf_prog_get_fd_by_id,
        bpf_prog_get_info_by_fd, iter_prog_ids, retry_with_verifier_logs, EbpfLoadProgramAttrs,
        SyscallError,
    },
    util::{bytes_of_bpf_name, KernelVersion},
    VerifierLogLevel,
};

/// Error type returned when working with programs.
#[derive(Debug, Error)]
pub enum ProgramError {
    /// The program is already loaded.
    #[error("the program is already loaded")]
    AlreadyLoaded,

    /// The program is not loaded.
    #[error("the program is not loaded")]
    NotLoaded,

    /// The program is already attached.
    #[error("the program was already attached")]
    AlreadyAttached,

    /// The program is not attached.
    #[error("the program is not attached")]
    NotAttached,

    /// Loading the program failed.
    #[error("the BPF_PROG_LOAD syscall failed. Verifier output: {verifier_log}")]
    LoadError {
        /// The [`io::Error`] returned by the `BPF_PROG_LOAD` syscall.
        #[source]
        io_error: io::Error,
        /// The error log produced by the kernel verifier.
        verifier_log: VerifierLog,
    },

    /// A syscall failed.
    #[error(transparent)]
    SyscallError(#[from] SyscallError),

    /// The network interface does not exist.
    #[error("unknown network interface {name}")]
    UnknownInterface {
        /// interface name
        name: String,
    },

    /// The program is not of the expected type.
    #[error("unexpected program type")]
    UnexpectedProgramType,

    /// A map error occurred while loading or attaching a program.
    #[error(transparent)]
    MapError(#[from] MapError),

    /// An error occurred while working with a [`KProbe`].
    #[error(transparent)]
    KProbeError(#[from] KProbeError),

    /// An error occurred while working with an [`UProbe`].
    #[error(transparent)]
    UProbeError(#[from] UProbeError),

    // /// An error occurred while working with a [`TracePoint`].
    // #[error(transparent)]
    // TracePointError(#[from] TracePointError),
    //
    // /// An error occurred while working with a [`SocketFilter`].
    // #[error(transparent)]
    // SocketFilterError(#[from] SocketFilterError),
    //
    // /// An error occurred while working with an [`Xdp`] program.
    // #[error(transparent)]
    // XdpError(#[from] XdpError),
    //
    // /// An error occurred while working with a TC program.
    // #[error(transparent)]
    // TcError(#[from] TcError),
    /// An error occurred while working with an [`Extension`] program.
    #[error(transparent)]
    ExtensionError(#[from] ExtensionError),

    /// An error occurred while working with BTF.
    #[error(transparent)]
    Btf(#[from] BtfError),

    /// The program is not attached.
    #[error("the program name `{name}` is invalid")]
    InvalidName {
        /// program name
        name: String,
    },

    /// An error occurred while working with IO.
    #[error(transparent)]
    IOError(#[from] io::Error),
}

/// A [`Program`] file descriptor.
#[derive(Debug)]
pub struct ProgramFd(OwnedFd);

impl ProgramFd {
    /// Creates a new instance that shares the same underlying file description as [`self`].
    pub fn try_clone(&self) -> io::Result<Self> {
        let Self(inner) = self;
        let inner = inner.try_clone()?;
        Ok(Self(inner))
    }
}

impl AsFd for ProgramFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        let Self(fd) = self;
        fd.as_fd()
    }
}

macro_rules! impl_fd {
    ($($struct_name:ident),+ $(,)?) => {
        $(
            impl $struct_name {
                /// Returns the file descriptor of this Program.
                pub fn fd(&self) -> Result<&ProgramFd, ProgramError> {
                    self.data.fd()
                }
            }
        )+
    }
}
impl_fd!(KProbe, Extension,);

macro_rules! impl_program_unload {
    ($($struct_name:ident),+ $(,)?) => {
        $(
            impl $struct_name {
                /// Unloads the program from the kernel.
                ///
                /// Links will be detached before unloading the program.  Note
                /// that owned links obtained using `take_link()` will not be
                /// detached.
                pub fn unload(&mut self) -> Result<(), ProgramError> {
                    info!("Unloading program for {:?}", self.name());
                    unload_program(&mut self.data)
                }
            }

            impl Drop for $struct_name {
                fn drop(&mut self) {
                    let _ = self.unload();
                }
            }
        )+
    }
}

impl_program_unload!(KProbe, Extension,);

macro_rules! impl_program_pin{
    ($($struct_name:ident),+ $(,)?) => {
        $(
            impl $struct_name {
                /// Pins the program to a BPF filesystem.
                ///
                /// When a BPF object is pinned to a BPF filesystem it will remain loaded after
                /// Aya has unloaded the program.
                /// To remove the program, the file on the BPF filesystem must be removed.
                /// Any directories in the the path provided should have been created by the caller.
                pub fn pin<P: AsRef<Path>>(&mut self, path: P) -> Result<(), PinError> {
                    // self.data.path = Some(path.as_ref().to_path_buf());
                    // pin_program(&self.data, path)
                    log::error!("Pinning a program is not yet implemented.");
                    unimplemented!("Pins the program to a BPF filesystem.")
                }

                /// Removes the pinned link from the filesystem.
                pub fn unpin(self) -> Result<(), io::Error> {
                    // if let Some(path) = self.data.path.take() {
                    //     std::fs::remove_file(path)?;
                    // }
                    // Ok(())
                    unimplemented!("Removes the pinned link from the filesystem.")
                }
            }
        )+
    }
}

impl_program_pin!(KProbe, Extension,);

/// Returns information about a loaded program with the [`ProgramInfo`] structure.
///
/// This information is populated at load time by the kernel and can be used
/// to correlate a given [`Program`] to it's corresponding [`ProgramInfo`]
/// metadata.
macro_rules! impl_info {
    ($($struct_name:ident),+ $(,)?) => {
        $(
            impl $struct_name {
                /// Returns the file descriptor of this Program.
                pub fn info(&self) -> Result<ProgramInfo, ProgramError> {
                    let ProgramFd(fd) = self.fd()?;

                    ProgramInfo::new_from_fd(fd.as_fd())
                }
            }
        )+
    }
}

impl_info!(KProbe, Extension,);

macro_rules! impl_from_pin {
    ($($struct_name:ident),+ $(,)?) => {
        $(
            impl $struct_name {
                /// Creates a program from a pinned entry on a bpffs.
                ///
                /// Existing links will not be populated. To work with existing links you should use [`crate::programs::links::PinnedLink`].
                ///
                /// On drop, any managed links are detached and the program is unloaded. This will not result in
                /// the program being unloaded from the kernel if it is still pinned.
                pub fn from_pin<P: AsRef<Path>>(path: P) -> Result<Self, ProgramError> {
                    let data = ProgramData::from_pinned_path(path, VerifierLogLevel::default())?;
                    Ok(Self { data })
                }
            }
        )+
    }
}

// Use impl_from_pin if the program doesn't require additional data
impl_from_pin!(Extension,);

macro_rules! impl_try_from_program {
    ($($ty:ident),+ $(,)?) => {
        $(
            impl<'a> TryFrom<&'a Program> for &'a $ty {
                type Error = ProgramError;

                fn try_from(program: &'a Program) -> Result<&'a $ty, ProgramError> {
                    match program {
                        Program::$ty(p) => Ok(p),
                        _ => Err(ProgramError::UnexpectedProgramType),
                    }
                }
            }

            impl<'a> TryFrom<&'a mut Program> for &'a mut $ty {
                type Error = ProgramError;

                fn try_from(program: &'a mut Program) -> Result<&'a mut $ty, ProgramError> {
                    match program {
                        Program::$ty(p) => Ok(p),
                        _ => Err(ProgramError::UnexpectedProgramType),
                    }
                }
            }
        )+
    }
}
impl_try_from_program!(KProbe, Extension,);

/// eBPF program type.
#[derive(Debug)]
pub enum Program {
    /// A [`KProbe`] program
    KProbe(KProbe),
    /// A [`Extension`] program
    Extension(Extension),
}

impl Program {
    /// Returns the low level program type.
    pub fn prog_type(&self) -> bpf_prog_type {
        use aya_obj::generated::bpf_prog_type::*;
        match self {
            Self::KProbe(_) => BPF_PROG_TYPE_KPROBE,
            Self::Extension(_) => BPF_PROG_TYPE_EXT,
        }
    }

    /// Pin the program to the provided path
    pub fn pin<P: AsRef<Path>>(&mut self, path: P) -> Result<(), PinError> {
        match self {
            Self::KProbe(p) => p.pin(path),
            Self::Extension(p) => p.pin(path),
        }
    }

    /// Unloads the program from the kernel.
    pub fn unload(self) -> Result<(), ProgramError> {
        match self {
            Self::KProbe(mut p) => p.unload(),
            Self::Extension(mut p) => p.unload(),
        }
    }

    /// Returns the file descriptor of a program.
    ///
    /// Can be used to add a program to a [`crate::maps::ProgramArray`] or attach an [`Extension`] program.
    pub fn fd(&self) -> Result<&ProgramFd, ProgramError> {
        match self {
            Self::KProbe(p) => p.fd(),
            Self::Extension(p) => p.fd(),
        }
    }
    /// Returns information about a loaded program with the [`ProgramInfo`] structure.
    ///
    /// This information is populated at load time by the kernel and can be used
    /// to get kernel details for a given [`Program`].
    pub fn info(&self) -> Result<ProgramInfo, ProgramError> {
        match self {
            Self::KProbe(p) => p.info(),
            Self::Extension(p) => p.info(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct ProgramData<T: Link> {
    pub(crate) name: Option<String>,
    pub(crate) obj: Option<(obj::Program, obj::Function)>,
    pub(crate) fd: Option<ProgramFd>,
    pub(crate) links: LinkMap<T>,
    pub(crate) expected_attach_type: Option<bpf_attach_type>,
    pub(crate) attach_btf_obj_fd: Option<OwnedFd>,
    pub(crate) attach_btf_id: Option<u32>,
    pub(crate) attach_prog_fd: Option<ProgramFd>,
    pub(crate) btf_fd: Option<Arc<OwnedFd>>,
    pub(crate) verifier_log_level: VerifierLogLevel,
    pub(crate) path: Option<PathBuf>,
    pub(crate) flags: u32,
}

impl<T: Link> ProgramData<T> {
    pub(crate) fn new(
        name: Option<String>,
        obj: (obj::Program, obj::Function),
        btf_fd: Option<Arc<OwnedFd>>,
        verifier_log_level: VerifierLogLevel,
    ) -> Self {
        Self {
            name,
            obj: Some(obj),
            fd: None,
            links: LinkMap::new(),
            expected_attach_type: None,
            attach_btf_obj_fd: None,
            attach_btf_id: None,
            attach_prog_fd: None,
            btf_fd,
            verifier_log_level,
            path: None,
            flags: 0,
        }
    }
    pub(crate) fn from_bpf_prog_info(
        name: Option<String>,
        fd: OwnedFd,
        path: &Path,
        info: bpf_prog_info,
        verifier_log_level: VerifierLogLevel,
    ) -> Result<Self, ProgramError> {
        let attach_btf_id = if info.attach_btf_id > 0 {
            Some(info.attach_btf_id)
        } else {
            None
        };
        let attach_btf_obj_fd = (info.attach_btf_obj_id != 0)
            .then(|| bpf_btf_get_fd_by_id(info.attach_btf_obj_id))
            .transpose()?;

        Ok(Self {
            name,
            obj: None,
            fd: Some(ProgramFd(fd)),
            links: LinkMap::new(),
            expected_attach_type: None,
            attach_btf_obj_fd,
            attach_btf_id,
            attach_prog_fd: None,
            btf_fd: None,
            verifier_log_level,
            path: Some(path.to_path_buf()),
            flags: 0,
        })
    }

    pub(crate) fn from_pinned_path<P: AsRef<Path>>(
        path: P,
        verifier_log_level: VerifierLogLevel,
    ) -> Result<Self, ProgramError> {
        use std::os::unix::ffi::OsStrExt as _;

        // TODO: avoid this unwrap by adding a new error variant.
        let path_string = CString::new(path.as_ref().as_os_str().as_bytes()).unwrap();
        let fd = bpf_get_object(&path_string).map_err(|(_, io_error)| SyscallError {
            call: "bpf_obj_get",
            io_error,
        })?;

        let info = ProgramInfo::new_from_fd(fd.as_fd())?;
        let name = info.name_as_str().map(|s| s.to_string());
        Self::from_bpf_prog_info(name, fd, path.as_ref(), info.0, verifier_log_level)
    }
}

impl<T: Link> ProgramData<T> {
    fn fd(&self) -> Result<&ProgramFd, ProgramError> {
        self.fd.as_ref().ok_or(ProgramError::NotLoaded)
    }

    pub(crate) fn take_link(&mut self, link_id: T::Id) -> Result<T, ProgramError> {
        self.links.forget(link_id)
    }
}

/// Provides information about a loaded program, like name, id and statistics
#[derive(Debug)]
pub struct ProgramInfo(bpf_prog_info);

impl ProgramInfo {
    fn new_from_fd(fd: BorrowedFd<'_>) -> Result<Self, ProgramError> {
        let info = bpf_prog_get_info_by_fd(fd, &mut [])?;
        Ok(Self(info))
    }

    /// The name of the program as was provided when it was load. This is limited to 16 bytes
    pub fn name(&self) -> &[u8] {
        bytes_of_bpf_name(&self.0.name)
    }

    /// The name of the program as a &str. If the name was not valid unicode, None is returned.
    pub fn name_as_str(&self) -> Option<&str> {
        core::str::from_utf8(self.name()).ok()
    }

    /// The id for this program. Each program has a unique id.
    pub fn id(&self) -> u32 {
        self.0.id
    }

    /// The program tag.
    ///
    /// The program tag is a SHA sum of the program's instructions which be used as an alternative to
    /// [`Self::id()`]". A program's id can vary every time it's loaded or unloaded, but the tag
    /// will remain the same.
    pub fn tag(&self) -> u64 {
        u64::from_be_bytes(self.0.tag)
    }

    /// The program type as defined by the linux kernel enum
    /// [`bpf_prog_type`](https://elixir.bootlin.com/linux/v6.4.4/source/include/uapi/linux/bpf.h#L948).
    pub fn program_type(&self) -> u32 {
        self.0.type_
    }

    /// Returns true if the program is defined with a GPL-compatible license.
    pub fn gpl_compatible(&self) -> bool {
        self.0.gpl_compatible() != 0
    }

    /// The ids of the maps used by the program.
    pub fn map_ids(&self) -> Result<Vec<u32>, ProgramError> {
        let ProgramFd(fd) = self.fd()?;
        let mut map_ids = vec![0u32; self.0.nr_map_ids as usize];

        bpf_prog_get_info_by_fd(fd.as_fd(), &mut map_ids)?;

        Ok(map_ids)
    }

    /// The btf id for the program.
    pub fn btf_id(&self) -> Option<NonZeroU32> {
        NonZeroU32::new(self.0.btf_id)
    }

    /// The size in bytes of the program's translated eBPF bytecode, which is
    /// the bytecode after it has been passed though the verifier where it was
    /// possibly modified by the kernel.
    pub fn size_translated(&self) -> u32 {
        self.0.xlated_prog_len
    }

    /// The size in bytes of the program's JIT-compiled machine code.
    pub fn size_jitted(&self) -> u32 {
        self.0.jited_prog_len
    }

    /// How much memory in bytes has been allocated and locked for the program.
    pub fn memory_locked(&self) -> Result<u32, ProgramError> {
        get_fdinfo(self.fd()?.as_fd(), "memlock")
    }

    /// The number of verified instructions in the program.
    ///
    /// This may be less than the total number of instructions in the compiled
    /// program due to dead code elimination in the verifier.
    pub fn verified_instruction_count(&self) -> u32 {
        self.0.verified_insns
    }

    // The time the program was loaded.
    // pub fn loaded_at(&self) -> SystemTime {
    //     boot_time() + Duration::from_nanos(self.0.load_time)
    // }

    /// Returns a file descriptor referencing the program.
    ///
    /// The returned file descriptor can be closed at any time and doing so does
    /// not influence the life cycle of the program.
    pub fn fd(&self) -> Result<ProgramFd, ProgramError> {
        let Self(info) = self;
        let fd = bpf_prog_get_fd_by_id(info.id)?;
        Ok(ProgramFd(fd))
    }

    /// Loads a program from a pinned path in bpffs.
    pub fn from_pin<P: AsRef<Path>>(path: P) -> Result<Self, ProgramError> {
        // use std::os::unix::ffi::OsStrExt as _;

        // TODO: avoid this unwrap by adding a new error variant.
        // let path_string = CString::new(path.as_ref().as_os_str().as_bytes()).unwrap();
        // let fd = bpf_get_object(&path_string).map_err(|(_, io_error)| SyscallError {
        //     call: "BPF_OBJ_GET",
        //     io_error,
        // })?;
        //
        // let info = bpf_prog_get_info_by_fd(fd.as_fd(), &mut [])?;
        // Ok(Self(info))
        unimplemented!("Loads a program from a pinned path in bpffs")
    }
}

fn unload_program<T: Link>(data: &mut ProgramData<T>) -> Result<(), ProgramError> {
    data.links.remove_all()?;
    data.fd
        .take()
        .ok_or(ProgramError::NotLoaded)
        .map(|ProgramFd { .. }| ())
}

fn load_program<T: Link>(
    prog_type: bpf_prog_type,
    data: &mut ProgramData<T>,
) -> Result<(), ProgramError> {
    let ProgramData {
        name,
        obj,
        fd,
        links: _,
        expected_attach_type,
        attach_btf_obj_fd,
        attach_btf_id,
        attach_prog_fd,
        btf_fd,
        verifier_log_level,
        path: _,
        flags,
    } = data;
    if fd.is_some() {
        return Err(ProgramError::AlreadyLoaded);
    }
    if obj.is_none() {
        // This program was loaded from a pin in bpffs
        return Err(ProgramError::AlreadyLoaded);
    }
    let obj = obj.as_ref().unwrap();
    let (
        obj::Program {
            license,
            kernel_version,
            ..
        },
        obj::Function {
            instructions,
            func_info,
            line_info,
            func_info_rec_size,
            line_info_rec_size,
            ..
        },
    ) = obj;

    let target_kernel_version =
        kernel_version.unwrap_or_else(|| KernelVersion::current().unwrap().code());

    let prog_name = if let Some(name) = name {
        let mut name = name.clone();
        if name.len() > 15 {
            name.truncate(15);
        }
        let prog_name = CString::new(name.clone())
            .map_err(|_| ProgramError::InvalidName { name: name.clone() })?;
        Some(prog_name)
    } else {
        None
    };

    let attr = EbpfLoadProgramAttrs {
        name: prog_name,
        ty: prog_type,
        insns: instructions,
        license,
        kernel_version: target_kernel_version,
        expected_attach_type: *expected_attach_type,
        prog_btf_fd: btf_fd.as_ref().map(|f| f.as_fd()),
        attach_btf_obj_fd: attach_btf_obj_fd.as_ref().map(|fd| fd.as_fd()),
        attach_btf_id: *attach_btf_id,
        attach_prog_fd: attach_prog_fd.as_ref().map(|fd| fd.as_fd()),
        func_info_rec_size: *func_info_rec_size,
        func_info: func_info.clone(),
        line_info_rec_size: *line_info_rec_size,
        line_info: line_info.clone(),
        flags: *flags,
    };

    let (ret, verifier_log) = retry_with_verifier_logs(10, |logger| {
        bpf_load_program(&attr, logger, *verifier_log_level)
    });

    match ret {
        Ok(prog_fd) => {
            *fd = Some(ProgramFd(prog_fd));
            Ok(())
        }
        Err((_, io_error)) => Err(ProgramError::LoadError {
            io_error,
            verifier_log,
        }),
    }
}

/// Returns an iterator over all loaded bpf programs.
///
/// This differs from [`crate::Ebpf::programs`] since it will return all programs
/// listed on the host system and not only programs a specific [`crate::Ebpf`] instance.
///
/// # Example
/// ```
/// # use aya::programs::loaded_programs;
///
/// for p in loaded_programs() {
///     match p {
///         Ok(program) => println!("{}", String::from_utf8_lossy(program.name())),
///         Err(e) => println!("Error iterating programs: {:?}", e),
///     }
/// }
/// ```
///
/// # Errors
///
/// Returns [`ProgramError::SyscallError`] if any of the syscalls required to either get
/// next program id, get the program fd, or the [`ProgramInfo`] fail. In cases where
/// iteration can't be performed, for example the caller does not have the necessary privileges,
/// a single item will be yielded containing the error that occurred.
pub fn loaded_programs() -> impl Iterator<Item = Result<ProgramInfo, ProgramError>> {
    iter_prog_ids()
        .map(|id| {
            let id = id?;
            bpf_prog_get_fd_by_id(id)
        })
        .map(|fd| {
            let fd = fd?;
            bpf_prog_get_info_by_fd(fd.as_fd(), &mut [])
        })
        .map(|result| result.map(ProgramInfo).map_err(Into::into))
}
