use crate::{
    arch::MMArch,
    filesystem::{
        page_cache::PageCache,
        tmpfs::{create_unlinked_shmem_file, TmpfsShmemFile},
        vfs::{
            file::{File, FileFlags},
            InodeId,
        },
    },
    ipc::id::IpcIdAllocator,
    libs::mutex::Mutex,
    mm::MemoryManagementArch,
    process::{
        cred::{capable, ns_capable, CAPFlags, Cred, Kgid, Kuid},
        namespace::{
            ipc_namespace::IpcNamespace,
            user_namespace::{map_id_down, map_id_up, UserNamespace},
        },
        resource::RLimitID,
        ProcessManager, RawPid,
    },
    time::PosixTimeSpec,
};
use alloc::sync::Arc;
use core::{
    fmt,
    hash::{Hash, Hasher},
    sync::atomic::{AtomicU64, Ordering},
};
use hashbrown::HashMap;
use num::ToPrimitive;
use system_error::SystemError;

/// 用于创建新的私有IPC对象
pub const IPC_PRIVATE: ShmKey = ShmKey::new(0);
const DEFAULT_OVERFLOW_ID: u32 = 65534;

int_like!(ShmId, usize);
int_like!(ShmKey, usize);

static NEXT_SYSV_SHM_ATTACH_ID: AtomicU64 = AtomicU64::new(1);

lazy_static::lazy_static! {
    static ref SYSV_SHM_MEMLOCK_ACCOUNT: Mutex<HashMap<SysVShmMemlockAccountKey, usize>> =
        Mutex::new(HashMap::new());
}

pub type SysVShmBackingRef = Arc<dyn SysVShmBacking>;

pub trait SysVShmBacking: fmt::Debug + Send + Sync {
    fn inode_id(&self) -> InodeId;

    fn open_file(&self, readonly: bool) -> Result<Arc<File>, SystemError>;

    fn resident_pages(&self) -> Result<usize, SystemError>;

    fn set_locked(&self, locked: bool) -> (Arc<PageCache>, bool);
}

impl SysVShmBacking for TmpfsShmemFile {
    fn inode_id(&self) -> InodeId {
        TmpfsShmemFile::inode_id(self)
    }

    fn open_file(&self, readonly: bool) -> Result<Arc<File>, SystemError> {
        let flags = if readonly {
            FileFlags::O_LARGEFILE
        } else {
            FileFlags::O_RDWR | FileFlags::O_LARGEFILE
        };
        Ok(Arc::new(File::new(self.inode(), flags)?))
    }

    fn resident_pages(&self) -> Result<usize, SystemError> {
        self.page_cache().manager().pages_count()
    }

    fn set_locked(&self, locked: bool) -> (Arc<PageCache>, bool) {
        TmpfsShmemFile::set_locked(self, locked)
    }
}

#[derive(Clone, Copy, Eq)]
struct SysVShmMemlockAccountKey {
    user_ns: usize,
    uid: usize,
}

impl SysVShmMemlockAccountKey {
    fn new(user_ns: &Arc<UserNamespace>, uid: usize) -> Self {
        Self {
            user_ns: Arc::as_ptr(user_ns) as usize,
            uid,
        }
    }
}

impl PartialEq for SysVShmMemlockAccountKey {
    fn eq(&self, other: &Self) -> bool {
        self.user_ns == other.user_ns && self.uid == other.uid
    }
}

impl Hash for SysVShmMemlockAccountKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.user_ns.hash(state);
        self.uid.hash(state);
    }
}

impl fmt::Debug for SysVShmMemlockAccountKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SysVShmMemlockAccountKey")
            .field("user_ns", &format_args!("{:#x}", self.user_ns))
            .field("uid", &self.uid)
            .finish()
    }
}

bitflags! {
    pub struct ShmFlags:u32{
        const PERM_MASK = 0o777;
        const SHM_RDONLY = 0o10000;
        const SHM_RND = 0o20000;
        const SHM_REMAP = 0o40000;
        const SHM_EXEC = 0o100000;
        const SHM_HUGETLB = 0o4000;

        const IPC_CREAT = 0o1000;
        const IPC_EXCL = 0o2000;

        const SHM_DEST = 0o1000;
        const SHM_LOCKED = 0o2000;
    }
}

/// 管理共享内存段信息的操作码
#[derive(Eq, Clone, Copy)]
pub enum ShmCtlCmd {
    /// 标记删除共享内存段（只有在映射计数为0时才真正删除）
    IpcRmid = 0,
    /// 设置KernIpcPerm选项
    IpcSet = 1,
    /// 获取ShmIdDs
    IpcStat = 2,
    /// 查看ShmMetaData
    IpcInfo = 3,

    /// 不允许共享内存段被置换出物理内存
    ShmLock = 11,
    /// 允许共享内存段被置换出物理内存
    ShmUnlock = 12,
    /// 查看ShmMetaData
    ShmStat = 13,
    /// 查看ShmInfo
    ShmInfo = 14,
    /// 查看ShmMetaData
    ShmtStatAny = 15,

    Default,
}

impl From<usize> for ShmCtlCmd {
    fn from(cmd: usize) -> ShmCtlCmd {
        match cmd {
            0 => Self::IpcRmid,
            1 => Self::IpcSet,
            2 => Self::IpcStat,
            3 => Self::IpcInfo,
            11 => Self::ShmLock,
            12 => Self::ShmUnlock,
            13 => Self::ShmStat,
            14 => Self::ShmInfo,
            15 => Self::ShmtStatAny,
            _ => Self::Default,
        }
    }
}

impl fmt::Display for ShmCtlCmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShmCtlCmd::IpcRmid => write!(f, "IPC_RMID"),
            ShmCtlCmd::IpcSet => write!(f, "IPC_SET"),
            ShmCtlCmd::IpcStat => write!(f, "IPC_STAT"),
            ShmCtlCmd::IpcInfo => write!(f, "IPC_INFO"),
            ShmCtlCmd::ShmLock => write!(f, "SHM_LOCK"),
            ShmCtlCmd::ShmUnlock => write!(f, "SHM_UNLOCK"),
            ShmCtlCmd::ShmStat => write!(f, "SHM_STAT"),
            ShmCtlCmd::ShmInfo => write!(f, "SHM_INFO"),
            ShmCtlCmd::ShmtStatAny => write!(f, "SHM_STAT_ANY"),
            ShmCtlCmd::Default => write!(f, "DEFAULT (Invalid Cmd)"),
        }
    }
}

impl PartialEq for ShmCtlCmd {
    fn eq(&self, other: &ShmCtlCmd) -> bool {
        *self as usize == *other as usize
    }
}

pub struct SysVShmAttach {
    attach_id: u64,
    ipcns: Arc<IpcNamespace>,
    shmid: ShmId,
    backing_inode_id: InodeId,
    size: usize,
    attach_file: Arc<File>,
}

impl fmt::Debug for SysVShmAttach {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SysVShmAttach")
            .field("attach_id", &self.attach_id)
            .field("shmid", &self.shmid)
            .field("backing_inode_id", &self.backing_inode_id)
            .finish_non_exhaustive()
    }
}

impl SysVShmAttach {
    pub fn new(
        ipcns: Arc<IpcNamespace>,
        shmid: ShmId,
        backing_inode_id: InodeId,
        size: usize,
        attach_file: Arc<File>,
    ) -> Arc<Self> {
        Arc::new(Self {
            attach_id: NEXT_SYSV_SHM_ATTACH_ID.fetch_add(1, Ordering::Relaxed),
            ipcns,
            shmid,
            backing_inode_id,
            size,
            attach_file,
        })
    }

    pub fn attach_id(&self) -> u64 {
        self.attach_id
    }

    pub fn shmid(&self) -> ShmId {
        self.shmid
    }

    pub fn backing_inode_id(&self) -> InodeId {
        self.backing_inode_id
    }

    pub fn size(&self) -> usize {
        self.size
    }

    pub fn attach_file(&self) -> Arc<File> {
        self.attach_file.clone()
    }

    pub fn open_vma(&self) -> Result<(), SystemError> {
        let mut guard = self.ipcns.shm.lock();
        guard.attach_open(self.shmid, self.backing_inode_id)
    }

    pub fn close_vma(&self) {
        let destroy = {
            let mut guard = self.ipcns.shm.lock();
            guard.attach_close(self.shmid, self.backing_inode_id)
        };
        if let Some(destroy) = destroy {
            destroy.finish_or_log("SysV SHM close_vma destroy cleanup");
        }
    }
}

pub struct SysVShmAttachGuard {
    ipcns: Arc<IpcNamespace>,
    shmid: ShmId,
    backing: SysVShmBackingRef,
    backing_inode_id: InodeId,
    size: usize,
    active: bool,
}

impl fmt::Debug for SysVShmAttachGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SysVShmAttachGuard")
            .field("shmid", &self.shmid)
            .field("backing_inode_id", &self.backing_inode_id)
            .field("size", &self.size)
            .field("active", &self.active)
            .finish_non_exhaustive()
    }
}

impl SysVShmAttachGuard {
    fn new(
        ipcns: Arc<IpcNamespace>,
        shmid: ShmId,
        backing: SysVShmBackingRef,
        backing_inode_id: InodeId,
        size: usize,
    ) -> Self {
        Self {
            ipcns,
            shmid,
            backing,
            backing_inode_id,
            size,
            active: true,
        }
    }

    pub fn size(&self) -> usize {
        self.size
    }

    pub fn create_attach(&self, readonly: bool) -> Result<Arc<SysVShmAttach>, SystemError> {
        let attach_file = self.backing.open_file(readonly)?;
        Ok(SysVShmAttach::new(
            self.ipcns.clone(),
            self.shmid,
            self.backing_inode_id,
            self.size,
            attach_file,
        ))
    }

    pub fn finish(mut self) {
        self.release_pin();
    }

    fn release_pin(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        let destroy = {
            let mut guard = self.ipcns.shm.lock();
            guard.attach_end(self.shmid, self.backing_inode_id)
        };
        if let Some(destroy) = destroy {
            destroy.finish_or_log("SysV SHM attach guard destroy cleanup");
        }
    }
}

impl Drop for SysVShmAttachGuard {
    fn drop(&mut self) {
        self.release_pin();
    }
}

/// 共享内存管理器
#[derive(Debug)]
pub struct ShmManager {
    /// ShmId分配器
    id_allocator: IpcIdAllocator,
    /// 低位 IPC idx 映射共享内存信息表
    id2shm: HashMap<usize, KernelShm>,
    /// ShmKey映射ShmId表
    key2id: HashMap<ShmKey, ShmId>,
    /// SysV SHM namespace-wide allocated pages, matching Linux shm_tot/SHMALL accounting.
    total_pages: usize,
}

impl Default for ShmManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ShmManager {
    const IPC_READ: u32 = 0o4;
    const IPC_WRITE: u32 = 0o2;
    const IPC_EXEC: u32 = 0o1;

    pub fn new() -> Self {
        ShmManager {
            id_allocator: IpcIdAllocator::new(PosixShmMetaInfo::SHMMNI).unwrap(),
            id2shm: HashMap::new(),
            key2id: HashMap::new(),
            total_pages: 0,
        }
    }

    pub fn page_count_for_size(size: usize) -> Result<usize, SystemError> {
        let rounded = size
            .checked_add(MMArch::PAGE_SIZE - 1)
            .ok_or(SystemError::ENOSPC)?
            & !(MMArch::PAGE_SIZE - 1);
        Ok(rounded >> MMArch::PAGE_SHIFT)
    }

    pub fn validate_new_segment_size(&self, size: usize) -> Result<usize, SystemError> {
        if !(PosixShmMetaInfo::SHMMIN..=PosixShmMetaInfo::SHMMAX).contains(&size) {
            return Err(SystemError::EINVAL);
        }

        let numpages = Self::page_count_for_size(size)?;
        let total_pages_after = self
            .total_pages
            .checked_add(numpages)
            .ok_or(SystemError::ENOSPC)?;
        if total_pages_after > PosixShmMetaInfo::SHMALL {
            return Err(SystemError::ENOSPC);
        }

        Ok(numpages)
    }

    fn release_total_pages(&mut self, pages: usize) {
        if let Some(total_pages) = self.total_pages.checked_sub(pages) {
            self.total_pages = total_pages;
        } else {
            log::error!(
                "SysV SHM total_pages accounting underflow: total_pages={}, release={}",
                self.total_pages,
                pages
            );
            debug_assert!(
                false,
                "SysV SHM total_pages accounting underflow: total_pages={}, release={}",
                self.total_pages, pages
            );
            self.total_pages = 0;
        }
    }

    /// # 添加共享内存段
    ///
    /// ## 参数
    ///
    /// - `key`: 共享内存键值
    /// - `size`: 共享内存大小
    /// - `shmflg`: 共享内存标志
    ///
    /// ## 返回值
    ///
    /// 成功：共享内存id
    /// 失败：对应错误码
    pub fn add_prepared(
        &mut self,
        key: ShmKey,
        size: usize,
        shmflg: ShmFlags,
        backing: SysVShmBackingRef,
        numpages: usize,
    ) -> Result<usize, SystemError> {
        let expected_numpages = self.validate_new_segment_size(size)?;
        if expected_numpages != numpages {
            return Err(SystemError::EINVAL);
        }
        let total_pages_after = self
            .total_pages
            .checked_add(numpages)
            .ok_or(SystemError::ENOSPC)?;
        let ipc_id = self.id_allocator.alloc()?;
        let shm_id = ShmId::new(ipc_id.raw);

        // 创建共享内存段信息结构体
        let current_cred = ProcessManager::current_pcb().cred();
        let kern_ipc_perm = KernIpcPerm::new_with_cred(
            shm_id,
            key,
            current_cred,
            shmflg & ShmFlags::PERM_MASK,
            ipc_id.seq,
        );
        let shm_kernel = KernelShm::new(kern_ipc_perm, backing, size, numpages);

        // 更新共享内存管理器相关映射表
        if key != IPC_PRIVATE {
            self.key2id.insert(key, shm_id);
        }
        self.id2shm.insert(ipc_id.idx, shm_kernel);
        self.total_pages = total_pages_after;

        return Ok(shm_id.data());
    }

    pub fn create_default_backing(size: usize) -> Result<SysVShmBackingRef, SystemError> {
        Ok(create_unlinked_shmem_file(size)?)
    }

    pub fn contains_key(&self, key: &ShmKey) -> Option<&ShmId> {
        self.key2id.get(key)
    }

    pub fn get_by_shmid_checked(&self, id: ShmId) -> Result<&KernelShm, SystemError> {
        let decoded = IpcIdAllocator::decode(id.data())?;
        let kernel_shm = self.id2shm.get(&decoded.idx).ok_or(SystemError::EINVAL)?;
        if kernel_shm.kern_ipc_perm.id != id || kernel_shm.kern_ipc_perm.seq != decoded.seq {
            return Err(SystemError::EINVAL);
        }

        Ok(kernel_shm)
    }

    pub fn get_by_shmid_checked_mut(&mut self, id: ShmId) -> Result<&mut KernelShm, SystemError> {
        let decoded = IpcIdAllocator::decode(id.data())?;
        let kernel_shm = self
            .id2shm
            .get_mut(&decoded.idx)
            .ok_or(SystemError::EINVAL)?;
        if kernel_shm.kern_ipc_perm.id != id || kernel_shm.kern_ipc_perm.seq != decoded.seq {
            return Err(SystemError::EINVAL);
        }

        Ok(kernel_shm)
    }

    fn get_by_index_for_shm_stat(&self, idx: usize) -> Result<&KernelShm, SystemError> {
        if idx > IpcIdAllocator::IPC_ID_IDX_MASK {
            return Err(SystemError::EINVAL);
        }
        self.id2shm.get(&idx).ok_or(SystemError::EINVAL)
    }

    fn get_by_attach_token_mut(
        &mut self,
        id: ShmId,
        backing_inode_id: InodeId,
    ) -> Result<&mut KernelShm, SystemError> {
        let decoded = IpcIdAllocator::decode(id.data())?;
        let kernel_shm = self
            .id2shm
            .get_mut(&decoded.idx)
            .ok_or(SystemError::EINVAL)?;
        if kernel_shm.kern_ipc_perm.id != id || kernel_shm.kern_ipc_perm.seq != decoded.seq {
            return Err(SystemError::EINVAL);
        }
        if kernel_shm.backing_inode_id != backing_inode_id {
            return Err(SystemError::EINVAL);
        }

        Ok(kernel_shm)
    }

    pub fn free_key(&mut self, key: &ShmKey) {
        self.key2id.remove(key);
    }

    pub fn free_id(&mut self, id: &ShmId) -> Option<KernelShmDestroy> {
        let Ok(decoded) = IpcIdAllocator::decode(id.data()) else {
            return None;
        };
        let current = self.id2shm.get(&decoded.idx)?;
        if current.kern_ipc_perm.id != *id || current.kern_ipc_perm.seq != decoded.seq {
            return None;
        }
        if let Some(shm) = self.id2shm.remove(&decoded.idx) {
            self.release_total_pages(shm.numpages);
            self.id_allocator.free_idx(decoded.idx);
            self.key2id.remove(&shm.kern_ipc_perm.key);
            return Some(KernelShmDestroy::new(shm));
        }
        None
    }

    fn cred_in_group(cred: &Cred, gid: Kgid) -> bool {
        cred.fsgid == gid
            || cred.groups.contains(&gid)
            || cred
                .group_info
                .as_ref()
                .map(|group_info| group_info.gids.contains(&gid))
                .unwrap_or(false)
    }

    fn ipc_permission(
        kern_ipc_perm: &KernIpcPerm,
        requested: u32,
        target_user_ns: &Arc<UserNamespace>,
    ) -> Result<(), SystemError> {
        let requested = ((requested >> 6) | (requested >> 3) | requested) & 0o7;
        if requested == 0 {
            return Ok(());
        }

        let cred = ProcessManager::current_pcb().cred();
        let mut granted = kern_ipc_perm.mode.bits();
        if cred.euid == kern_ipc_perm.cuid || cred.euid == kern_ipc_perm.uid {
            granted >>= 6;
        } else if Self::cred_in_group(&cred, kern_ipc_perm.cgid)
            || Self::cred_in_group(&cred, kern_ipc_perm.gid)
        {
            granted >>= 3;
        }

        if (requested & !(granted & 0o7)) != 0
            && !ns_capable(target_user_ns, CAPFlags::CAP_IPC_OWNER)
        {
            return Err(SystemError::EACCES);
        }

        Ok(())
    }

    fn check_control_permission(
        kern_ipc_perm: &KernIpcPerm,
        target_user_ns: &Arc<UserNamespace>,
    ) -> Result<(), SystemError> {
        let cred = ProcessManager::current_pcb().cred();
        if cred.euid == kern_ipc_perm.cuid
            || cred.euid == kern_ipc_perm.uid
            || ns_capable(target_user_ns, CAPFlags::CAP_SYS_ADMIN)
        {
            Ok(())
        } else {
            Err(SystemError::EPERM)
        }
    }

    fn check_lock_permission(
        kern_ipc_perm: &KernIpcPerm,
        target_user_ns: &Arc<UserNamespace>,
    ) -> Result<(), SystemError> {
        let cred = ProcessManager::current_pcb().cred();
        if cred.euid == kern_ipc_perm.cuid
            || cred.euid == kern_ipc_perm.uid
            || ns_capable(target_user_ns, CAPFlags::CAP_IPC_LOCK)
        {
            Ok(())
        } else {
            Err(SystemError::EPERM)
        }
    }

    pub(crate) fn charge_memlock_for_shm(size: usize) -> Result<SysVShmMemlockToken, SystemError> {
        let pcb = ProcessManager::current_pcb();
        let rlimit = pcb.get_rlimit(RLimitID::Memlock).rlim_cur;
        let bytes = size
            .checked_add(MMArch::PAGE_SIZE - 1)
            .ok_or(SystemError::ENOMEM)?
            & !(MMArch::PAGE_SIZE - 1);
        let cred = pcb.cred();
        let uid = cred.uid.data();
        let account_user_ns = cred.user_ns.clone();
        let account_key = SysVShmMemlockAccountKey::new(&account_user_ns, uid);
        let mut guard = SYSV_SHM_MEMLOCK_ACCOUNT.lock();
        let current = guard.get(&account_key).copied().unwrap_or(0);
        let next = current.checked_add(bytes).ok_or(SystemError::ENOMEM)?;
        if (next as u128) > rlimit as u128 && !capable(CAPFlags::CAP_IPC_LOCK) {
            return Err(SystemError::ENOMEM);
        }

        guard.insert(account_key, next);
        Ok(SysVShmMemlockToken {
            account_user_ns,
            account_key,
            bytes,
        })
    }

    pub fn check_existing_key_permission(
        &self,
        id: ShmId,
        shmflg: ShmFlags,
    ) -> Result<(), SystemError> {
        let kernel_shm = self.get_by_shmid_checked(id)?;
        let requested = shmflg.bits() & ShmFlags::PERM_MASK.bits();
        let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
        Self::ipc_permission(&kernel_shm.kern_ipc_perm, requested, &target_user_ns)
    }

    fn maybe_take_destroy_candidate_locked(&mut self, id: ShmId) -> Option<KernelShmDestroy> {
        let decoded = IpcIdAllocator::decode(id.data()).ok()?;
        let shm = self.id2shm.get(&decoded.idx)?;
        if shm.kern_ipc_perm.id != id || shm.kern_ipc_perm.seq != decoded.seq {
            return None;
        }
        if !shm.mode().contains(ShmFlags::SHM_DEST) || shm.nattch() != 0 || shm.pin_count != 0 {
            return None;
        }

        let shm = self.id2shm.remove(&decoded.idx)?;
        self.release_total_pages(shm.numpages);
        self.id_allocator.free_idx(decoded.idx);
        self.key2id.remove(&shm.kern_ipc_perm.key);
        Some(KernelShmDestroy::new(shm))
    }

    pub fn attach_begin(
        &mut self,
        ipcns: Arc<IpcNamespace>,
        id: ShmId,
        readonly: bool,
        executable: bool,
    ) -> Result<SysVShmAttachGuard, SystemError> {
        let kernel_shm = self.get_by_shmid_checked_mut(id)?;
        let mut requested = Self::IPC_READ;
        if !readonly {
            requested |= Self::IPC_WRITE;
        }
        if executable {
            requested |= Self::IPC_EXEC;
        }
        Self::ipc_permission(&kernel_shm.kern_ipc_perm, requested, &ipcns.user_ns)?;
        let backing = kernel_shm.backing.clone();
        let backing_inode_id = kernel_shm.backing_inode_id;
        let size = kernel_shm.size();
        kernel_shm.pin_count = kernel_shm
            .pin_count
            .checked_add(1)
            .ok_or(SystemError::EOVERFLOW)?;
        Ok(SysVShmAttachGuard::new(
            ipcns,
            id,
            backing,
            backing_inode_id,
            size,
        ))
    }

    pub fn attach_open(&mut self, id: ShmId, backing_inode_id: InodeId) -> Result<(), SystemError> {
        let kernel_shm = self.get_by_attach_token_mut(id, backing_inode_id)?;
        kernel_shm.update_atim();
        kernel_shm.increase_count()?;
        Ok(())
    }

    fn attach_close(&mut self, id: ShmId, backing_inode_id: InodeId) -> Option<KernelShmDestroy> {
        let kernel_shm = match self.get_by_attach_token_mut(id, backing_inode_id) {
            Ok(kernel_shm) => kernel_shm,
            Err(err) => {
                log::error!(
                    "SysV SHM attach_close token mismatch for shmid={}, backing_inode_id={:?}: {:?}",
                    id.data(),
                    backing_inode_id,
                    err
                );
                debug_assert!(
                    false,
                    "SysV SHM attach_close token mismatch for shmid={}",
                    id.data()
                );
                return None;
            }
        };
        kernel_shm.update_dtim();
        kernel_shm.decrease_count();
        self.maybe_take_destroy_candidate_locked(id)
    }

    fn attach_end(&mut self, id: ShmId, backing_inode_id: InodeId) -> Option<KernelShmDestroy> {
        let kernel_shm = match self.get_by_attach_token_mut(id, backing_inode_id) {
            Ok(kernel_shm) => kernel_shm,
            Err(err) => {
                log::error!(
                    "SysV SHM attach_end token mismatch for shmid={}, backing_inode_id={:?}: {:?}",
                    id.data(),
                    backing_inode_id,
                    err
                );
                debug_assert!(
                    false,
                    "SysV SHM attach_end token mismatch for shmid={}",
                    id.data()
                );
                return None;
            }
        };
        if let Some(pin_count) = kernel_shm.pin_count.checked_sub(1) {
            kernel_shm.pin_count = pin_count;
        } else {
            log::error!("SysV SHM pin_count underflow for shmid={}", id.data());
            debug_assert!(
                false,
                "SysV SHM pin_count underflow for shmid={}",
                id.data()
            );
            kernel_shm.pin_count = 0;
        }
        self.maybe_take_destroy_candidate_locked(id)
    }

    fn current_max_index(&self) -> usize {
        self.id2shm.keys().copied().max().unwrap_or(0)
    }

    pub fn ipc_info_data(&self) -> (usize, PosixShmMetaInfo) {
        (self.current_max_index(), PosixShmMetaInfo::new())
    }

    pub fn shm_info_data(&self) -> Result<(usize, PosixShmInfo), SystemError> {
        // 已使用id数量
        let used_ids = self.id2shm.len().to_i32().ok_or(SystemError::EOVERFLOW)?;
        let shm_rss = self.id2shm.values().try_fold(0usize, |acc, shm| {
            let resident = shm.backing.resident_pages()?;
            acc.checked_add(resident).ok_or(SystemError::EOVERFLOW)
        })?;
        let shm_info = PosixShmInfo::new(used_ids, self.total_pages, shm_rss, 0, 0, 0);
        Ok((self.current_max_index(), shm_info))
    }

    pub fn shm_stat_data(
        &self,
        id: ShmId,
        cmd: ShmCtlCmd,
    ) -> Result<(usize, PosixShmIdDs), SystemError> {
        let kernel_shm = match cmd {
            ShmCtlCmd::IpcStat => self.get_by_shmid_checked(id)?,
            ShmCtlCmd::ShmStat | ShmCtlCmd::ShmtStatAny => {
                self.get_by_index_for_shm_stat(id.data())?
            }
            _ => return Err(SystemError::EINVAL),
        };
        if cmd != ShmCtlCmd::ShmtStatAny {
            let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
            Self::ipc_permission(&kernel_shm.kern_ipc_perm, Self::IPC_READ, &target_user_ns)?;
        }
        let kern_ipc_perm = &kernel_shm.kern_ipc_perm;
        let current_user_ns = ProcessManager::current_user_ns();
        let shm_perm = kern_ipc_perm.to_posix(&current_user_ns)?;
        let shm_segsz = kernel_shm.shm_size;
        let shm_atime = kernel_shm.shm_atim.tv_sec;
        let shm_dtime = kernel_shm.shm_dtim.tv_sec;
        let shm_ctime = kernel_shm.shm_ctim.tv_sec;
        let shm_cpid = kernel_shm
            .shm_cprid
            .data()
            .to_u32()
            .ok_or(SystemError::EOVERFLOW)?;
        let shm_lpid = kernel_shm
            .shm_lprid
            .data()
            .to_u32()
            .ok_or(SystemError::EOVERFLOW)?;
        let shm_map_count = kernel_shm.map_count();
        let shm_id_ds = PosixShmIdDs {
            shm_perm,
            shm_segsz,
            shm_atime,
            shm_dtime,
            shm_ctime,
            shm_cpid,
            shm_lpid,
            shm_map_count,
            _unused1: 0,
            _unused2: 0,
        };

        let r: usize = if cmd == ShmCtlCmd::IpcStat {
            0
        } else {
            kern_ipc_perm.id.data()
        };

        return Ok((r, shm_id_ds));
    }

    pub fn ipc_set(&mut self, id: ShmId, shm_id_ds: PosixShmIdDs) -> Result<usize, SystemError> {
        let kernel_shm = self.get_by_shmid_checked_mut(id)?;
        let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
        Self::check_control_permission(&kernel_shm.kern_ipc_perm, &target_user_ns)?;

        let current_user_ns = ProcessManager::current_user_ns();
        kernel_shm.copy_from(shm_id_ds, &current_user_ns)?;

        return Ok(0);
    }

    pub(crate) fn ipc_rmid(&mut self, id: ShmId) -> Result<Option<KernelShmDestroy>, SystemError> {
        let key = {
            let kernel_shm = self.get_by_shmid_checked_mut(id)?;
            let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
            Self::check_control_permission(&kernel_shm.kern_ipc_perm, &target_user_ns)?;
            // Linux do_shm_rmid() marks an attached segment as SHM_DEST and
            // hides its key, but does not refresh shm_ctim. IPC_SET remains
            // the metadata-changing operation that updates shm_ctim.
            kernel_shm.set_mode_no_ctime(ShmFlags::SHM_DEST, true);
            let key = kernel_shm.kern_ipc_perm.key;
            kernel_shm.kern_ipc_perm.key = IPC_PRIVATE;
            key
        };
        self.free_key(&key);
        Ok(self.maybe_take_destroy_candidate_locked(id))
    }

    pub(crate) fn shm_lock_begin(&mut self, id: ShmId) -> Result<ShmLockBegin, SystemError> {
        let kernel_shm = self.get_by_shmid_checked_mut(id)?;
        let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
        Self::check_lock_permission(&kernel_shm.kern_ipc_perm, &target_user_ns)?;
        let has_target_ns_cap = ns_capable(&target_user_ns, CAPFlags::CAP_IPC_LOCK);
        if ProcessManager::current_pcb()
            .get_rlimit(RLimitID::Memlock)
            .rlim_cur
            == 0
            && !has_target_ns_cap
        {
            return Err(SystemError::EPERM);
        }
        if kernel_shm.mode().contains(ShmFlags::SHM_LOCKED) {
            return Ok(ShmLockBegin::Done(None));
        }

        Ok(ShmLockBegin::NeedCharge {
            size: kernel_shm.shm_size,
        })
    }

    pub(crate) fn shm_lock_commit(
        &mut self,
        id: ShmId,
        token: SysVShmMemlockToken,
    ) -> Result<Option<(Arc<PageCache>, bool)>, SystemError> {
        let kernel_shm = match self.get_by_shmid_checked_mut(id) {
            Ok(kernel_shm) => kernel_shm,
            Err(err) => {
                token.release();
                return Err(err);
            }
        };
        let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
        if let Err(err) = Self::check_lock_permission(&kernel_shm.kern_ipc_perm, &target_user_ns) {
            token.release();
            return Err(err);
        }
        if kernel_shm.mode().contains(ShmFlags::SHM_LOCKED) {
            token.release();
            return Ok(None);
        }

        let page_cache = kernel_shm.backing.set_locked(true);
        kernel_shm.memlock_token = Some(token);
        kernel_shm.set_mode_no_ctime(ShmFlags::SHM_LOCKED, true);
        Ok(Some(page_cache))
    }

    pub fn shm_unlock(&mut self, id: ShmId) -> Result<Option<(Arc<PageCache>, bool)>, SystemError> {
        let kernel_shm = self.get_by_shmid_checked_mut(id)?;
        let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
        Self::check_lock_permission(&kernel_shm.kern_ipc_perm, &target_user_ns)?;
        if !kernel_shm.mode().contains(ShmFlags::SHM_LOCKED) {
            return Ok(None);
        }

        let page_cache = kernel_shm.backing.set_locked(false);
        kernel_shm.set_mode_no_ctime(ShmFlags::SHM_LOCKED, false);
        if let Some(token) = kernel_shm.memlock_token.take() {
            token.release();
        }
        Ok(Some(page_cache))
    }
}

#[derive(Debug)]
pub(crate) enum ShmLockBegin {
    Done(Option<(Arc<PageCache>, bool)>),
    NeedCharge { size: usize },
}

pub(crate) struct SysVShmMemlockToken {
    account_user_ns: Arc<UserNamespace>,
    account_key: SysVShmMemlockAccountKey,
    bytes: usize,
}

impl fmt::Debug for SysVShmMemlockToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SysVShmMemlockToken")
            .field(
                "account_user_ns",
                &format_args!("{:#x}", Arc::as_ptr(&self.account_user_ns) as usize),
            )
            .field("account_key", &self.account_key)
            .field("bytes", &self.bytes)
            .finish()
    }
}

impl SysVShmMemlockToken {
    fn release(self) {
        let mut guard = SYSV_SHM_MEMLOCK_ACCOUNT.lock();
        if let Some(current) = guard.get_mut(&self.account_key) {
            if let Some(next) = current.checked_sub(self.bytes) {
                *current = next;
            } else {
                log::error!(
                    "SysV SHM memlock accounting underflow: current={}, release={}",
                    *current,
                    self.bytes
                );
                debug_assert!(
                    false,
                    "SysV SHM memlock accounting underflow: current={}, release={}",
                    *current, self.bytes
                );
                *current = 0;
            }
            if *current == 0 {
                guard.remove(&self.account_key);
            }
        }
    }
}

/// 共享内存段信息
#[derive(Debug)]
pub struct KernelShm {
    /// 权限信息
    kern_ipc_perm: KernIpcPerm,
    /// 共享内存段底层 backing。当前默认实现为 tmpfs，但 SysV IPC 层只依赖此抽象。
    backing: SysVShmBackingRef,
    /// backing inode id cached at creation time; read under shm SpinLock without touching tmpfs mutexes.
    backing_inode_id: InodeId,
    /// 共享内存段大小(bytes)，注意是用户指定的大小（未经过页面对齐）
    shm_size: usize,
    /// 共享内存段页面数，用于 SysV SHMALL/shm_tot accounting.
    numpages: usize,
    /// live SysV VMA descriptor 计数
    nattch: usize,
    /// attach 正在建立过程中的临时 pin 计数
    pin_count: usize,
    /// 最后一次 attach 的时间
    shm_atim: PosixTimeSpec,
    /// 最后一次 detach 的时间
    shm_dtim: PosixTimeSpec,
    /// 最后一次更改信息的时间
    shm_ctim: PosixTimeSpec,
    /// 创建者进程id
    shm_cprid: RawPid,
    /// 最后操作者进程id (这里的操作者是指最后一次 attach 或 detach 操作的进程，创建共享内存段的进程不算操作者)
    shm_lprid: RawPid,
    /// SysV SHM_LOCK memlock accounting token.
    memlock_token: Option<SysVShmMemlockToken>,
}

impl KernelShm {
    pub fn new(
        kern_ipc_perm: KernIpcPerm,
        backing: SysVShmBackingRef,
        shm_size: usize,
        numpages: usize,
    ) -> Self {
        let shm_cprid = ProcessManager::current_pid();
        KernelShm {
            kern_ipc_perm,
            backing_inode_id: backing.inode_id(),
            backing,
            shm_size,
            numpages,
            nattch: 0,
            pin_count: 0,
            shm_atim: PosixTimeSpec::new(0, 0),
            shm_dtim: PosixTimeSpec::new(0, 0),
            shm_ctim: PosixTimeSpec::now(),
            shm_cprid,
            shm_lprid: RawPid::new(0), // 初始值为0，表示尚未有进程对这个共享内存段执行 attach 或 detach 操作，对齐 Linux 行为
            memlock_token: None,
        }
    }

    pub fn size(&self) -> usize {
        self.shm_size
    }

    /// 更新最后 attach 时间
    pub fn update_atim(&mut self) {
        // 更新最后一次 attach 时间
        self.shm_atim = PosixTimeSpec::now();

        // 更新最后操作当前共享内存的进程ID
        self.shm_lprid = ProcessManager::current_pid();
    }

    /// 更新最后 detach 时间
    pub fn update_dtim(&mut self) {
        // 更新最后一次 detach 时间
        self.shm_dtim = PosixTimeSpec::now();

        // 更新最后操作当前共享内存的进程ID
        self.shm_lprid = ProcessManager::current_pid();
    }

    /// 更新最后一次修改信息的时间
    pub fn update_ctim(&mut self) {
        // 更新最后一次修改信息的时间
        self.shm_ctim = PosixTimeSpec::now();
    }

    /// 共享内存段的映射计数（有多少个不同的VMA映射）
    pub fn map_count(&self) -> usize {
        self.nattch
    }

    pub fn nattch(&self) -> usize {
        self.nattch
    }

    pub fn copy_from(
        &mut self,
        shm_id_ds: PosixShmIdDs,
        user_ns: &Arc<UserNamespace>,
    ) -> Result<(), SystemError> {
        let uid = KernIpcPerm::make_kuid(user_ns, shm_id_ds.uid())?;
        let gid = KernIpcPerm::make_kgid(user_ns, shm_id_ds.gid())?;
        let perm_bits = ShmFlags::from_bits_truncate(shm_id_ds.mode()) & ShmFlags::PERM_MASK;
        self.kern_ipc_perm.uid = uid;
        self.kern_ipc_perm.gid = gid;
        self.kern_ipc_perm.mode.remove(ShmFlags::PERM_MASK);
        self.kern_ipc_perm.mode.insert(perm_bits);
        self.update_ctim();
        Ok(())
    }

    pub fn set_mode(&mut self, shmflg: ShmFlags, set: bool) {
        self.set_mode_no_ctime(shmflg, set);
        self.update_ctim();
    }

    pub fn set_mode_no_ctime(&mut self, shmflg: ShmFlags, set: bool) {
        if set {
            self.kern_ipc_perm.mode.insert(shmflg);
        } else {
            self.kern_ipc_perm.mode.remove(shmflg);
        }
    }

    pub fn mode(&self) -> &ShmFlags {
        &self.kern_ipc_perm.mode
    }

    pub fn increase_count(&mut self) -> Result<(), SystemError> {
        self.nattch = self.nattch.checked_add(1).ok_or(SystemError::EOVERFLOW)?;
        Ok(())
    }

    pub fn decrease_count(&mut self) {
        assert!(self.nattch > 0, "nattch is zero");
        self.nattch -= 1;
    }

    fn prepare_destroy_cleanup(&mut self) -> Option<(Arc<PageCache>, bool)> {
        let reclassify = if self.mode().contains(ShmFlags::SHM_LOCKED) {
            let reclassify = self.backing.set_locked(false);
            self.set_mode_no_ctime(ShmFlags::SHM_LOCKED, false);
            Some(reclassify)
        } else {
            None
        };
        if let Some(token) = self.memlock_token.take() {
            token.release();
        }
        reclassify
    }
}

pub struct KernelShmDestroy {
    shm: Option<KernelShm>,
}

impl fmt::Debug for KernelShmDestroy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KernelShmDestroy")
            .field("pending", &self.shm.is_some())
            .finish()
    }
}

impl KernelShmDestroy {
    fn new(shm: KernelShm) -> Self {
        Self { shm: Some(shm) }
    }

    pub fn finish(mut self) {
        let Some(mut shm) = self.shm.take() else {
            return;
        };
        let reclassify = shm.prepare_destroy_cleanup();
        drop(shm);
        if let Some((page_cache, old_mapping_unevictable)) = reclassify {
            page_cache.reclassify_unevictable_pages(old_mapping_unevictable);
        }
    }

    pub fn finish_or_log(self, context: &str) {
        let _ = context;
        self.finish();
    }
}

impl Drop for KernelShmDestroy {
    fn drop(&mut self) {
        if self.shm.is_some() {
            log::error!("KernelShmDestroy dropped without explicit finish()");
            debug_assert!(false, "KernelShmDestroy dropped without explicit finish()");
        }
    }
}

impl Drop for KernelShm {
    fn drop(&mut self) {
        if let Some(token) = self.memlock_token.take() {
            log::error!(
                "KernelShm dropped with unreleased SysV SHM_LOCK memlock token; releasing token"
            );
            token.release();
        }
        if self.mode().contains(ShmFlags::SHM_LOCKED) {
            log::error!("KernelShm dropped while SHM_LOCKED; explicit destroy cleanup was skipped");
            debug_assert!(
                false,
                "KernelShm dropped while SHM_LOCKED; explicit destroy cleanup was skipped"
            );
        }
    }
}

/// 共享内存段权限信息
#[derive(Debug)]
pub struct KernIpcPerm {
    /// 共享内存段id
    id: ShmId,
    /// 共享内存段键值，由创建共享内存用户指定
    key: ShmKey,
    /// 共享内存段拥有者用户id（kernel-global uid）
    uid: Kuid,
    /// 共享内存段拥有者所在组id（kernel-global gid）
    gid: Kgid,
    /// 共享内存段创建者用户id（kernel-global uid）
    cuid: Kuid,
    /// 共享内存段创建者所在组id（kernel-global gid）
    cgid: Kgid,
    /// 共享内存段权限模式
    mode: ShmFlags,
    /// 序列号：用于在 ShmId 被重用的时候进行区分
    /// TODO: 目前尚未实现该机制，默认为0，具体实现方式见 (https://github.com/DragonOS-Community/DragonOS/issues/1678)
    seq: usize,
}

impl KernIpcPerm {
    pub fn new_with_cred(
        id: ShmId,
        key: ShmKey,
        cred: Arc<Cred>,
        mode: ShmFlags,
        seq: usize,
    ) -> Self {
        KernIpcPerm {
            id,
            key,
            uid: cred.euid,
            gid: cred.egid,
            cuid: cred.euid,
            cgid: cred.egid,
            mode,
            seq,
        }
    }

    fn make_kuid(user_ns: &Arc<UserNamespace>, uid: u32) -> Result<Kuid, SystemError> {
        let inner = user_ns.inner.lock();
        map_id_down(&inner.uid_map, uid)
            .map(|uid| Kuid::new(uid as usize))
            .ok_or(SystemError::EINVAL)
    }

    fn make_kgid(user_ns: &Arc<UserNamespace>, gid: u32) -> Result<Kgid, SystemError> {
        let inner = user_ns.inner.lock();
        map_id_down(&inner.gid_map, gid)
            .map(|gid| Kgid::new(gid as usize))
            .ok_or(SystemError::EINVAL)
    }

    fn kuid_to_user(user_ns: &Arc<UserNamespace>, kuid: Kuid) -> u32 {
        let Ok(uid) = u32::try_from(kuid.data()) else {
            return DEFAULT_OVERFLOW_ID;
        };
        let inner = user_ns.inner.lock();
        map_id_up(&inner.uid_map, uid).unwrap_or(DEFAULT_OVERFLOW_ID)
    }

    fn kgid_to_user(user_ns: &Arc<UserNamespace>, kgid: Kgid) -> u32 {
        let Ok(gid) = u32::try_from(kgid.data()) else {
            return DEFAULT_OVERFLOW_ID;
        };
        let inner = user_ns.inner.lock();
        map_id_up(&inner.gid_map, gid).unwrap_or(DEFAULT_OVERFLOW_ID)
    }

    fn to_posix(&self, user_ns: &Arc<UserNamespace>) -> Result<PosixIpcPerm, SystemError> {
        let key = self.key.data() as u32 as i32;

        Ok(PosixIpcPerm {
            key,
            uid: Self::kuid_to_user(user_ns, self.uid),
            gid: Self::kgid_to_user(user_ns, self.gid),
            cuid: Self::kuid_to_user(user_ns, self.cuid),
            cgid: Self::kgid_to_user(user_ns, self.cgid),
            mode: self.mode.bits(),
            seq: self.seq.to_i32().ok_or(SystemError::EOVERFLOW)?,
            _pad1: 0,
            _unused1: 0,
            _unused2: 0,
        })
    }
}

/// 共享内存元信息，符合POSIX标准
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PosixShmMetaInfo {
    /// 最大共享内存段的大小(bytes)
    shmmax: usize,
    /// 最小共享内存段的大小(bytes)
    shmmin: usize,
    /// 最大共享内存标识符数量
    shmmni: usize,
    /// 单个进程可以拥有的最大共享内存段的数量，和最大共享内存标识符数量相同
    shmseg: usize,
    /// 所有共享内存段总共可以使用的最大内存量(pages)
    shmall: usize,
    _unused1: usize,
    _unused2: usize,
    _unused3: usize,
    _unused4: usize,
}

impl PosixShmMetaInfo {
    /// 最小共享内存段的大小(bytes)
    pub const SHMMIN: usize = 1;
    /// 最大共享内存标识符数量
    pub const SHMMNI: usize = 4096;
    /// 最大共享内存段的大小(bytes)
    pub const SHMMAX: usize = usize::MAX - (1 << 24);
    /// 所有共享内存段总共可以使用的最大内存量(pages)
    pub const SHMALL: usize = usize::MAX - (1 << 24);
    /// 单个进程可以拥有的最大共享内存段的数量，和最大共享内存标识符数量相同
    pub const SHMSEG: usize = 4096;

    pub fn new() -> Self {
        PosixShmMetaInfo {
            shmmax: Self::SHMMAX,
            shmmin: Self::SHMMIN,
            shmmni: Self::SHMMNI,
            shmseg: Self::SHMSEG,
            shmall: Self::SHMALL,
            _unused1: 0,
            _unused2: 0,
            _unused3: 0,
            _unused4: 0,
        }
    }
}

/// 共享内存信息，符合POSIX标准
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PosixShmInfo {
    /// 已使用id数
    used_ids: i32,
    /// 显式填充，避免 copy_to_user 时泄漏 repr(C) 隐式 padding。
    _pad0: i32,
    /// 共享内存总量(pages)
    shm_tot: usize,
    /// 保留在内存中的共享内存大小
    shm_rss: usize,
    /// 被置换出的共享内存大小
    shm_swp: usize,
    /// 尝试置换次数
    swap_attempts: usize,
    /// 成功置换次数
    swap_successes: usize,
}

impl PosixShmInfo {
    pub fn new(
        used_ids: i32,
        shm_tot: usize,
        shm_rss: usize,
        shm_swp: usize,
        swap_attempts: usize,
        swap_successes: usize,
    ) -> Self {
        PosixShmInfo {
            used_ids,
            _pad0: 0,
            shm_tot,
            shm_rss,
            shm_swp,
            swap_attempts,
            swap_successes,
        }
    }
}

/// 共享内存段属性信息，符合POSIX标准
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct PosixShmIdDs {
    /// 共享内存段权限
    shm_perm: PosixIpcPerm,
    /// 共享内存段大小(bytes)
    shm_segsz: usize,
    /// 最后一次 attach 的时间
    shm_atime: i64,
    /// 最后一次 detach 的时间
    shm_dtime: i64,
    /// 最后一次更改信息的时间
    shm_ctime: i64,
    /// 创建者进程id
    shm_cpid: u32,
    /// 最后操作者进程id
    shm_lpid: u32,
    /// 链接数
    shm_map_count: usize,
    _unused1: usize,
    _unused2: usize,
}

impl PosixShmIdDs {
    pub fn uid(&self) -> u32 {
        self.shm_perm.uid
    }

    pub fn gid(&self) -> u32 {
        self.shm_perm.gid
    }

    pub fn mode(&self) -> u32 {
        self.shm_perm.mode
    }
}

/// 共享内存段权限，符合POSIX标准
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct PosixIpcPerm {
    /// IPC对象键值
    key: i32,
    /// 当前用户id
    uid: u32,
    /// 当前用户组id
    gid: u32,
    /// 创建者用户id
    cuid: u32,
    /// 创建者组id
    cgid: u32,
    /// 权限
    mode: u32,
    /// 序列号
    seq: i32,
    _pad1: i32,
    _unused1: usize,
    _unused2: usize,
}
