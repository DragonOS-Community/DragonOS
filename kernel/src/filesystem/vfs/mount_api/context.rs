//! An fsopen(2) open-file-description and its configuration state.
//!
//! The fd table owns the File; dup/fork therefore share one context.  No
//! mount topology lock is held while a filesystem maker builds its root.

use alloc::{string::String, sync::Arc, vec::Vec};
use core::{any::Any, fmt};
use system_error::SystemError;

use crate::{
    filesystem::anon_inode::{anon_inode_metadata, anon_inode_path, AnonInodeFs},
    filesystem::vfs::{
        file::{File, FileFlags, FilePrivateData},
        filesystem_maker,
        inode_lifecycle::{InodeRetentionGuard, InodeRetentionKind},
        mount::{DetachedMountTree, MountFS, MountFlags, MountSnapshotGuard},
        produce_fs_in_context, FileSystem, FsCreationContext, FsconfigPreparedData, IndexNode,
        InodeMode, Metadata,
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
    ReconfParams,
    Reconfiguring,
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
    sb_flags_mask: MountFlags,
    fs: Option<Arc<dyn FileSystem>>,
    prepared: Option<FsconfigPreparedData>,
    target: Option<ReconfigureTarget>,
}

/// Retain the selected backing root and active SB, but not a mount busy pin.
/// This deliberately permits ordinary umount while an fspick fd stays open.
struct ReconfigureTarget {
    _root: InodeRetentionGuard,
    _superblock: MountSnapshotGuard,
    mount: Arc<MountFS>,
}

impl ReconfigureTarget {
    fn new(mount: Arc<MountFS>) -> Result<Self, SystemError> {
        let superblock = mount.try_pin_snapshot()?;
        let root =
            InodeRetentionGuard::new(mount.root_inner_inode(), InodeRetentionKind::Operation)?;
        Ok(Self {
            _root: root,
            _superblock: superblock,
            mount,
        })
    }
}

impl FsContextState {
    fn new(phase: Phase) -> Self {
        Self {
            phase,
            source: None,
            options: Vec::new(),
            data_len: 0,
            sb_flags: MountFlags::empty(),
            sb_flags_mask: MountFlags::empty(),
            fs: None,
            prepared: None,
            target: None,
        }
    }

    /// Linux vfs_clean_context: infallible cleanup with cumulative mask kept.
    fn clean_parameters(&mut self) {
        self.source = None;
        self.options.clear();
        self.data_len = 0;
        self.sb_flags = MountFlags::empty();
        self.prepared = None;
        self.phase = Phase::AwaitingReconf;
    }
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
        state: Mutex::new(FsContextState::new(Phase::CreateParams)),
    })
    .map_err(|_| SystemError::ENOMEM)?;

    install_context_fd(context, cloexec)
}

/// The caller keeps the resolved mount alive until the SB-only pin is taken.
pub fn pick_fs_context(mount: Arc<MountFS>, cloexec: bool) -> Result<i32, SystemError> {
    let target = ReconfigureTarget::new(mount)?;
    let mut state = FsContextState::new(Phase::ReconfParams);
    let fs_type = String::from(target.mount.inner_filesystem().name());
    state.target = Some(target);
    let context = Arc::try_new(FsContext {
        fs_type,
        creation: FsCreationContext::current(),
        state: Mutex::new(state),
    })
    .map_err(|_| SystemError::ENOMEM)?;
    install_context_fd(context, cloexec)
}

fn install_context_fd(context: Arc<FsContext>, cloexec: bool) -> Result<i32, SystemError> {
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

fn common_superblock_option(key: &str, state: &mut FsContextState) -> bool {
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
        state.sb_flags.insert(flag);
    } else {
        state.sb_flags.remove(flag);
    }
    state.sb_flags_mask.insert(flag);
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
    if state.phase == Phase::AwaitingReconf {
        // Registered makers use fresh legacy parameter storage. There is no
        // fallible allocation to perform until a parameter is actually added.
        state.phase = Phase::ReconfParams;
    }
    match cmd {
        FsConfigCommand::SetFlag(key) => {
            if !matches!(state.phase, Phase::CreateParams | Phase::ReconfParams) {
                return Err(SystemError::EBUSY);
            }
            if key == "source" {
                return Err(SystemError::EINVAL);
            }
            if common_superblock_option(&key, &mut state) {
                return Ok(());
            }
            if state.phase == Phase::ReconfParams {
                state
                    .target
                    .as_ref()
                    .ok_or(SystemError::EINVAL)?
                    .mount
                    .inner_filesystem()
                    .validate_reconfigure_parameter(&key, None)?;
            } else {
                filesystem_maker(&context.fs_type)
                    .ok_or(SystemError::ENODEV)?
                    .validate_fsconfig_parameter(&key, None)?;
            }
            append_legacy_option(&mut state, key, None)
        }
        FsConfigCommand::SetString(key, value) => {
            if !matches!(state.phase, Phase::CreateParams | Phase::ReconfParams) {
                return Err(SystemError::EBUSY);
            }
            if key == "source" {
                if state.source.is_some() {
                    return Err(SystemError::EINVAL);
                }
                state.source = Some(value);
                return Ok(());
            }
            if common_superblock_option(&key, &mut state) {
                return Ok(());
            }
            // Creation preparers can retain lower paths or other resources for
            // a new filesystem. Reconfiguration belongs to the existing
            // backend and must not run a new-filesystem preparer.
            let prepared = if state.phase == Phase::CreateParams {
                let maker = filesystem_maker(&context.fs_type).ok_or(SystemError::ENODEV)?;
                maker.validate_fsconfig_parameter(&key, Some(&value))?;
                maker.prepare_fsconfig_string(&key, &value, state.prepared.as_ref())?
            } else {
                state
                    .target
                    .as_ref()
                    .ok_or(SystemError::EINVAL)?
                    .mount
                    .inner_filesystem()
                    .validate_reconfigure_parameter(&key, Some(&value))?;
                None
            };
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
        FsConfigCommand::Reconfigure => {
            if state.phase != Phase::ReconfParams {
                return Err(SystemError::EBUSY);
            }
            state.phase = Phase::Reconfiguring;
            let result = (|| {
                let target = state.target.as_ref().ok_or(SystemError::EINVAL)?;
                let superblock = target.mount.super_block_state();
                // Execute with the caller's capability, not creation.cred.
                if !ns_capable(superblock.owner_user_ns(), CAPFlags::CAP_SYS_ADMIN) {
                    return Err(SystemError::EPERM);
                }
                let raw_data = encode_legacy_options(&state)?;
                let _umount = superblock.umount_write();
                let prepared = super::reconfigure::prepare_reconfigure_locked(
                    &target.mount,
                    crate::filesystem::vfs::FsReconfigureRequest {
                        sb_flags: state.sb_flags,
                        sb_flags_mask: state.sb_flags_mask,
                        raw_data: raw_data.as_deref(),
                        oldapi: false,
                    },
                )?;
                prepared.commit();
                Ok(())
            })();
            match result {
                Ok(()) => {
                    state.clean_parameters();
                    Ok(())
                }
                Err(error) => {
                    state.phase = Phase::Failed;
                    Err(error)
                }
            }
        }
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
    if state.fs.is_none() && state.target.is_none() {
        return Err(SystemError::EINVAL);
    }
    if state.phase != Phase::AwaitingMount {
        return Err(SystemError::EBUSY);
    }
    let fs = state.fs.as_ref().ok_or(SystemError::EINVAL)?.clone();
    let result = create(
        fs,
        state.source.clone(),
        state.sb_flags,
        context.creation.cred.user_ns.clone(),
    )?;
    // Acquire these while the newly created tree still owns its live mount.
    // No fallible operation may follow cleanup of the successfully used fd.
    state.target = Some(ReconfigureTarget::new(result.root())?);
    state.fs = None;
    state.clean_parameters();
    Ok(result)
}

#[derive(Debug)]
struct FsContextInode {
    metadata: Metadata,
}

impl FsContextInode {
    fn new() -> Self {
        let metadata = anon_inode_metadata(InodeMode::S_IRUSR | InodeMode::S_IWUSR);
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
        AnonInodeFs::instance()
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        Ok(self.metadata.clone())
    }

    fn stat_mode(&self, metadata: &Metadata) -> InodeMode {
        metadata.mode
    }

    fn absolute_path(&self) -> Result<String, SystemError> {
        Ok(anon_inode_path("[fscontext]"))
    }
}
