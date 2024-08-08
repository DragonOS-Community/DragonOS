use std::{
    collections::{hash_map::Entry, HashMap},
    ffi::CString,
    io,
    os::fd::{AsFd, AsRawFd, OwnedFd, RawFd},
    path::{Path, PathBuf},
};

use thiserror::Error;

/// A Link.
pub trait Link: core::fmt::Debug + 'static {
    /// Unique Id
    type Id: core::fmt::Debug + core::hash::Hash + Eq + PartialEq;

    /// Returns the link id
    fn id(&self) -> Self::Id;

    /// Detaches the LinkOwnedLink is gone... but this doesn't work :(
    fn detach(self) -> Result<(), ProgramError>;
}

#[derive(Debug)]
pub(crate) struct LinkMap<T: Link> {
    links: HashMap<T::Id, T>,
}

impl<T: Link> LinkMap<T> {
    pub(crate) fn new() -> Self {
        Self {
            links: HashMap::new(),
        }
    }

    pub(crate) fn insert(&mut self, link: T) -> Result<T::Id, ProgramError> {
        let id = link.id();

        match self.links.entry(link.id()) {
            Entry::Occupied(_) => return Err(ProgramError::AlreadyAttached),
            Entry::Vacant(e) => e.insert(link),
        };

        Ok(id)
    }

    pub(crate) fn remove(&mut self, link_id: T::Id) -> Result<(), ProgramError> {
        self.links
            .remove(&link_id)
            .ok_or(ProgramError::NotAttached)?
            .detach()
    }

    pub(crate) fn remove_all(&mut self) -> Result<(), ProgramError> {
        for (_, link) in self.links.drain() {
            link.detach()?;
        }
        Ok(())
    }

    pub(crate) fn forget(&mut self, link_id: T::Id) -> Result<T, ProgramError> {
        self.links.remove(&link_id).ok_or(ProgramError::NotAttached)
    }
}

impl<T: Link> Drop for LinkMap<T> {
    fn drop(&mut self) {
        let _ = self.remove_all();
    }
}

/// The identifier of an `FdLink`.
#[derive(Debug, Hash, Eq, PartialEq)]
pub struct FdLinkId(pub(crate) RawFd);

/// A file descriptor link.
///
/// Fd links are returned directly when attaching some program types (for
/// instance [`crate::programs::cgroup_skb::CgroupSkb`]), or can be obtained by
/// converting other link types (see the `TryFrom` implementations).
///
/// An important property of fd links is that they can be pinned. Pinning
/// can be used keep a link attached "in background" even after the program
/// that has created the link terminates.
///
/// # Example
///
///```no_run
/// # let mut bpf = Ebpf::load_file("ebpf_programs.o")?;
/// use aya::{Ebpf, programs::{links::FdLink, KProbe}};
///
/// let program: &mut KProbe = bpf.program_mut("intercept_wakeups").unwrap().try_into()?;
/// program.load()?;
/// let link_id = program.attach("try_to_wake_up", 0)?;
/// let link = program.take_link(link_id).unwrap();
/// let fd_link: FdLink = link.try_into().unwrap();
/// fd_link.pin("/sys/fs/bpf/intercept_wakeups_link").unwrap();
///
/// # Ok::<(), aya::EbpfError>(())
/// ```
#[derive(Debug)]
pub struct FdLink {
    pub(crate) fd: OwnedFd,
}
impl FdLink {
    pub(crate) fn new(fd: OwnedFd) -> Self {
        Self { fd }
    }
    /// Pins the link to a BPF file system.
    ///
    /// When a link is pinned it will remain attached even after the link instance is dropped,
    /// and will only be detached once the pinned file is removed. To unpin, see [`PinnedLink::unpin()`].
    ///
    /// The parent directories in the provided path must already exist before calling this method,
    /// and must be on a BPF file system (bpffs).
    ///
    /// # Example
    /// ```no_run
    /// # use aya::programs::{links::FdLink, Extension};
    /// # use std::convert::TryInto;
    /// # #[derive(thiserror::Error, Debug)]
    /// # enum Error {
    /// #     #[error(transparent)]
    /// #     Ebpf(#[from] aya::EbpfError),
    /// #     #[error(transparent)]
    /// #     Pin(#[from] aya::pin::PinError),
    /// #     #[error(transparent)]
    /// #     Program(#[from] aya::programs::ProgramError)
    /// # }
    /// # let mut bpf = aya::Ebpf::load(&[])?;
    /// # let prog: &mut Extension = bpf.program_mut("example").unwrap().try_into()?;
    /// let link_id = prog.attach()?;
    /// let owned_link = prog.take_link(link_id)?;
    /// let fd_link: FdLink = owned_link.into();
    /// let pinned_link = fd_link.pin("/sys/fs/bpf/example")?;
    /// # Ok::<(), Error>(())
    /// ```
    pub fn pin<P: AsRef<Path>>(self, path: P) -> Result<PinnedLink, PinError> {
        use std::os::unix::ffi::OsStrExt as _;

        let path = path.as_ref();
        let path_string = CString::new(path.as_os_str().as_bytes()).map_err(|error| {
            PinError::InvalidPinPath {
                path: path.into(),
                error,
            }
        })?;
        bpf_pin_object(self.fd.as_fd(), &path_string).map_err(|(_, io_error)| SyscallError {
            call: "BPF_OBJ_PIN",
            io_error,
        })?;
        Ok(PinnedLink::new(path.into(), self))
    }
}

impl Link for FdLink {
    type Id = FdLinkId;

    fn id(&self) -> Self::Id {
        FdLinkId(self.fd.as_raw_fd())
    }

    fn detach(self) -> Result<(), ProgramError> {
        // detach is a noop since it consumes self. once self is consumed, drop will be triggered
        // and the link will be detached.
        //
        // Other links don't need to do this since they use define_link_wrapper!, but FdLink is a
        // bit special in that it defines a custom ::new() so it can't use the macro.
        Ok(())
    }
}

#[derive(Error, Debug)]
/// Errors from operations on links.
pub enum LinkError {
    /// Invalid link.
    #[error("Invalid link")]
    InvalidLink,
    /// Syscall failed.
    #[error(transparent)]
    SyscallError(#[from] SyscallError),
}

/// A pinned file descriptor link.
///
/// This link has been pinned to the BPF filesystem. On drop, the file descriptor that backs
/// this link will be closed. Whether or not the program remains attached is dependent
/// on the presence of the file in BPFFS.
#[derive(Debug)]
pub struct PinnedLink {
    inner: FdLink,
    path: PathBuf,
}

impl PinnedLink {
    fn new(path: PathBuf, link: FdLink) -> Self {
        Self { inner: link, path }
    }

    /// Creates a [`crate::programs::links::PinnedLink`] from a valid path on bpffs.
    pub fn from_pin<P: AsRef<Path>>(path: P) -> Result<Self, LinkError> {
        use std::os::unix::ffi::OsStrExt as _;

        // TODO: avoid this unwrap by adding a new error variant.
        let path_string = CString::new(path.as_ref().as_os_str().as_bytes()).unwrap();
        let fd = bpf_get_object(&path_string).map_err(|(_, io_error)| {
            LinkError::SyscallError(SyscallError {
                call: "BPF_OBJ_GET",
                io_error,
            })
        })?;
        Ok(Self::new(path.as_ref().to_path_buf(), FdLink::new(fd)))
    }

    /// Removes the pinned link from the filesystem and returns an [`FdLink`].
    pub fn unpin(self) -> Result<FdLink, io::Error> {
        std::fs::remove_file(self.path)?;
        Ok(self.inner)
    }
}

macro_rules! define_link_wrapper {
    (#[$doc1:meta] $wrapper:ident, #[$doc2:meta] $wrapper_id:ident, $base:ident, $base_id:ident) => {
        #[$doc2]
        #[derive(Debug, Hash, Eq, PartialEq)]
        pub struct $wrapper_id($base_id);

        #[$doc1]
        #[derive(Debug)]
        pub struct $wrapper(Option<$base>);

        #[allow(dead_code)]
        // allow dead code since currently XDP is the only consumer of inner and
        // into_inner
        impl $wrapper {
            fn new(base: $base) -> $wrapper {
                $wrapper(Some(base))
            }

            fn inner(&self) -> &$base {
                self.0.as_ref().unwrap()
            }

            fn into_inner(mut self) -> $base {
                self.0.take().unwrap()
            }
        }

        impl Drop for $wrapper {
            fn drop(&mut self) {
                use crate::programs::links::Link;

                if let Some(base) = self.0.take() {
                    let _ = base.detach();
                }
            }
        }

        impl $crate::programs::Link for $wrapper {
            type Id = $wrapper_id;

            fn id(&self) -> Self::Id {
                $wrapper_id(self.0.as_ref().unwrap().id())
            }

            fn detach(mut self) -> Result<(), ProgramError> {
                self.0.take().unwrap().detach()
            }
        }

        impl From<$base> for $wrapper {
            fn from(b: $base) -> $wrapper {
                $wrapper(Some(b))
            }
        }

        impl From<$wrapper> for $base {
            fn from(mut w: $wrapper) -> $base {
                w.0.take().unwrap()
            }
        }
    };
}

pub(crate) use define_link_wrapper;

use crate::{
    pin::PinError,
    programs::ProgramError,
    sys::{bpf_get_object, bpf_pin_object, SyscallError},
};
