//! An fsopen(2) open-file-description and its configuration state.
//!
//! The fd table owns the File; dup/fork therefore share one context.  No
//! mount topology lock is held while a filesystem maker builds its root.

use alloc::{string::String, sync::Arc, vec::Vec};
use core::{any::Any, fmt};
use system_error::SystemError;

use crate::{
    filesystem::vfs::{
        file::{File, FileFlags, FilePrivateData},
        filesystem_maker,
        mount::{DetachedMountTree, MountFlags},
        produce_fs_in_context, FileSystem, FileType, FsCreationContext, FsInfo,
        FsconfigPreparedData, IndexNode, InodeMode, Magic, Metadata, SuperBlock,
    },
    libs::mutex::{Mutex, MutexGuard},
    mm::MemoryManagementArch,
    process::{
        cred::{capable, ns_capable, CAPFlags},
        namespace::user_namespace::{UserNamespace, INIT_USER_NAMESPACE},
        ProcessManager,
    },
};

const LEGACY_DATA_PAGE: usize = <crate::arch::MMArch as MemoryManagementArch>::PAGE_SIZE;

#[derive(Debug)]
pub enum FsConfigCommand {
    SetFlag(String),
    SetString(String, String),
    /// The current registered makers consume legacy flag/string parameters;
    /// binary, path, and fd parameters must not be coerced into that format.
    UnsupportedTyped,
    Create,
    CreateExcl,
    Reconfigure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    CreateParams,
    Creating,
    AwaitingMount,
    AwaitingReconf,
    Failed,
}

struct LegacyOption {
    key: String,
    value: Option<String>,
}

struct FsContextState {
    phase: Phase,
    source: Option<String>,
    options: Vec<LegacyOption>,
    data_len: usize,
    sb_flags: MountFlags,
    fs: Option<Arc<dyn FileSystem>>,
    prepared: Option<FsconfigPreparedData>,
}

/// Only an Arc to this object is placed in FilePrivateData. The short-lived
/// file-private-data lock protects the type check; this lock serializes
/// fsconfig/fsmount and can span filesystem creation without holding a VFS
/// open-file lock or a mount topology lock.
pub struct FsContext {
    fs_type: String,
    creation: FsCreationContext,
    state: Mutex<FsContextState>,
}

impl fmt::Debug for FsContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FsContext")
            .field("fs_type", &self.fs_type)
            .field("phase", &self.state.lock().phase)
            .finish()
    }
}

fn context_from_fd(fd: i32) -> Result<Arc<FsContext>, SystemError> {
    let file = ProcessManager::current_pcb()
        .fd_table()
        .get_file_by_fd(fd)
        .ok_or(SystemError::EBADF)?;
    let private = file.private_data.lock();
    match &*private {
        FilePrivateData::FsContext(context) => Ok(context.clone()),
        _ => Err(SystemError::EINVAL),
    }
}

pub fn open_fs_context(fs_name: &str, cloexec: bool) -> Result<i32, SystemError> {
    // Linux checks may_mount() before flags, pathname, or filesystem lookup.
    let mnt_ns = ProcessManager::current_mntns();
    if !ns_capable(mnt_ns.user_ns(), CAPFlags::CAP_SYS_ADMIN) {
        return Err(SystemError::EPERM);
    }
    if filesystem_maker(fs_name).is_none() {
        return Err(SystemError::ENODEV);
    }

    let context = Arc::try_new(FsContext {
        fs_type: String::from(fs_name),
        creation: FsCreationContext::current(),
        state: Mutex::new(FsContextState {
            phase: Phase::CreateParams,
            source: None,
            options: Vec::new(),
            data_len: 0,
            sb_flags: MountFlags::empty(),
            fs: None,
            prepared: None,
        }),
    })
    .map_err(|_| SystemError::ENOMEM)?;

    let inode: Arc<dyn IndexNode> = Arc::new(FsContextInode::new());
    let file = File::new_with_private_data(
        inode,
        FileFlags::O_RDWR,
        FilePrivateData::FsContext(context),
    )?;
    let current = ProcessManager::current_pcb();
    current
        .fd_table()
        .alloc_fd(file, cloexec, current.nofile_soft_limit())
}

fn common_superblock_option(key: &str, flags: &mut MountFlags) -> bool {
    let (flag, set) = match key {
        "dirsync" => (MountFlags::DIRSYNC, true),
        "lazytime" => (MountFlags::LAZYTIME, true),
        "mand" => (MountFlags::MANDLOCK, true),
        "ro" => (MountFlags::RDONLY, true),
        "sync" => (MountFlags::SYNCHRONOUS, true),
        "async" => (MountFlags::SYNCHRONOUS, false),
        "nolazytime" => (MountFlags::LAZYTIME, false),
        "nomand" => (MountFlags::MANDLOCK, false),
        "rw" => (MountFlags::RDONLY, false),
        _ => return false,
    };
    if set {
        flags.insert(flag);
    } else {
        flags.remove(flag);
    }
    true
}

fn append_legacy_option(
    state: &mut FsContextState,
    key: String,
    value: Option<String>,
) -> Result<(), SystemError> {
    // A legacy maker receives comma-delimited text. Refuse data that would
    // change the parameter boundaries rather than silently reinterpret it.
    if key.is_empty()
        || key.len() > 255
        || key.bytes().any(|byte| byte == b',' || byte == 0)
        || value.as_ref().is_some_and(|value| {
            value.len() > 255 || value.bytes().any(|byte| byte == b',' || byte == 0)
        })
    {
        return Err(SystemError::EINVAL);
    }
    let option_len = key
        .len()
        .checked_add(value.as_ref().map_or(0, |value| 1 + value.len()))
        .ok_or(SystemError::EINVAL)?;
    if state
        .data_len
        .checked_add(option_len)
        .and_then(|len| len.checked_add(2))
        .ok_or(SystemError::EINVAL)?
        > LEGACY_DATA_PAGE
    {
        return Err(SystemError::EINVAL);
    }
    let new_len = state.data_len + usize::from(!state.options.is_empty()) + option_len;
    state
        .options
        .try_reserve(1)
        .map_err(|_| SystemError::ENOMEM)?;
    state.options.push(LegacyOption { key, value });
    state.data_len = new_len;
    Ok(())
}

fn encode_legacy_options(state: &FsContextState) -> Result<Option<String>, SystemError> {
    if state.options.is_empty() {
        return Ok(None);
    }
    let mut data = String::new();
    data.try_reserve(state.data_len)
        .map_err(|_| SystemError::ENOMEM)?;
    for option in &state.options {
        if !data.is_empty() {
            data.push(',');
        }
        data.push_str(&option.key);
        if let Some(value) = &option.value {
            data.push('=');
            data.push_str(value);
        }
    }
    Ok(Some(data))
}

pub fn configure_fs_context(fd: i32, cmd: FsConfigCommand) -> Result<(), SystemError> {
    let context = context_from_fd(fd)?;
    // Linux's legacy_fs_context_ops has no handler for these types. This
    // decision is independent of the configuration phase.
    if matches!(cmd, FsConfigCommand::UnsupportedTyped) {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    let mut state = context.state.lock();
    match cmd {
        FsConfigCommand::SetFlag(key) => {
            if state.phase != Phase::CreateParams {
                return Err(SystemError::EBUSY);
            }
            if key == "source" {
                return Err(SystemError::EINVAL);
            }
            if common_superblock_option(&key, &mut state.sb_flags) {
                return Ok(());
            }
            append_legacy_option(&mut state, key, None)
        }
        FsConfigCommand::SetString(key, value) => {
            if state.phase != Phase::CreateParams {
                return Err(SystemError::EBUSY);
            }
            if key == "source" {
                if state.source.is_some() {
                    return Err(SystemError::EINVAL);
                }
                state.source = Some(value);
                return Ok(());
            }
            if common_superblock_option(&key, &mut state.sb_flags) {
                return Ok(());
            }
            let prepared = filesystem_maker(&context.fs_type)
                .ok_or(SystemError::ENODEV)?
                .prepare_fsconfig_string(&key, &value, state.prepared.as_ref())?;
            append_legacy_option(&mut state, key, Some(value))?;
            state.prepared = prepared;
            Ok(())
        }
        FsConfigCommand::Create => {
            if state.phase != Phase::CreateParams {
                return Err(SystemError::EBUSY);
            }
            // None of the existing DragonOS makers promises the security
            // properties of Linux FS_USERNS_MOUNT yet. Require capability in
            // the initial user namespace until individual makers are audited.
            if !capable(CAPFlags::CAP_SYS_ADMIN) {
                return Err(SystemError::EPERM);
            }
            state.phase = Phase::Creating;
            let result = (|| {
                // Until a maker is explicitly audited for FS_USERNS_MOUNT,
                // do not let an fsfd opened in another user namespace create
                // a superblock owned by the caller of fsconfig(2).
                if !Arc::ptr_eq(&context.creation.cred.user_ns, &INIT_USER_NAMESPACE) {
                    return Err(SystemError::EPERM);
                }
                if !state.options.is_empty()
                    && !filesystem_maker(&context.fs_type)
                        .ok_or(SystemError::ENODEV)?
                        .supports_fsconfig_legacy_options()
                {
                    return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                }
                let raw_data = encode_legacy_options(&state)?;
                let mut creation = context.creation.clone();
                creation.fsconfig_prepared = state.prepared.clone();
                produce_fs_in_context(
                    &context.fs_type,
                    raw_data.as_deref(),
                    state.source.as_deref().unwrap_or(""),
                    state.sb_flags,
                    &creation,
                )
            })();
            match result {
                Ok(fs) => {
                    state.fs = Some(fs);
                    state.phase = Phase::AwaitingMount;
                    Ok(())
                }
                Err(error) => {
                    state.phase = Phase::Failed;
                    Err(error)
                }
            }
        }
        FsConfigCommand::CreateExcl => {
            if state.phase != Phase::CreateParams {
                Err(SystemError::EBUSY)
            } else if !capable(CAPFlags::CAP_SYS_ADMIN) {
                Err(SystemError::EPERM)
            } else {
                // Registered DragonOS makers use legacy create/reuse paths.
                Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
            }
        }
        FsConfigCommand::Reconfigure => Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        FsConfigCommand::UnsupportedTyped => unreachable!(),
    }
}

/// The callback must create the detached mount object. Once it succeeds the
/// context is consumed, even if subsequent fd creation or installation fails.
pub fn create_mount_from_fs_context(
    fd: i32,
    create: impl FnOnce(
        Arc<dyn FileSystem>,
        Option<String>,
        MountFlags,
        Arc<UserNamespace>,
    ) -> Result<Arc<DetachedMountTree>, SystemError>,
) -> Result<Arc<DetachedMountTree>, SystemError> {
    let context = context_from_fd(fd)?;
    let mut state = context.state.lock();
    match state.phase {
        Phase::AwaitingMount => {}
        Phase::AwaitingReconf => return Err(SystemError::EBUSY),
        Phase::CreateParams | Phase::Creating | Phase::Failed => return Err(SystemError::EINVAL),
    }
    let fs = state.fs.as_ref().ok_or(SystemError::EINVAL)?.clone();
    let result = create(
        fs,
        state.source.clone(),
        state.sb_flags,
        context.creation.cred.user_ns.clone(),
    )?;
    state.fs = None;
    state.phase = Phase::AwaitingReconf;
    Ok(result)
}

#[derive(Debug)]
struct FsContextFileSystem;

impl FileSystem for FsContextFileSystem {
    fn page_cache_writeback_domain(
        &self,
    ) -> Option<&Arc<crate::filesystem::page_cache::PageCacheWritebackDomain>> {
        None
    }

    fn root_inode(&self) -> Arc<dyn IndexNode> {
        Arc::new(FsContextInode::new())
    }

    fn info(&self) -> FsInfo {
        FsInfo {
            blk_dev_id: 0,
            max_name_len: 255,
        }
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "anon_inode"
    }

    fn super_block(&self) -> SuperBlock {
        SuperBlock::new(
            Magic::ANON_INODEFS_MAGIC,
            <crate::arch::MMArch as MemoryManagementArch>::PAGE_SIZE as u64,
            255,
        )
    }
}

lazy_static::lazy_static! {
    static ref FSCONTEXT_FS: Arc<FsContextFileSystem> = Arc::new(FsContextFileSystem);
}

#[derive(Debug)]
struct FsContextInode {
    metadata: Metadata,
}

impl FsContextInode {
    fn new() -> Self {
        let metadata = Metadata::new(
            FileType::File,
            InodeMode::S_IFREG | InodeMode::S_IRUSR | InodeMode::S_IWUSR,
        );
        Self { metadata }
    }
}

impl IndexNode for FsContextInode {
    fn is_stream(&self) -> bool {
        true
    }

    fn open(
        &self,
        _data: MutexGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn close(&self, _data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        Ok(())
    }

    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // Linux fscontext_read returns ENODATA when its diagnostic queue is
        // empty. DragonOS makers do not currently emit fs-context messages.
        Err(SystemError::ENODATA)
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EINVAL)
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        FSCONTEXT_FS.clone()
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        Ok(self.metadata.clone())
    }

    fn absolute_path(&self) -> Result<String, SystemError> {
        Ok(String::from("anon_inode:[fscontext]"))
    }
}
