use core::sync::atomic::{compiler_fence, AtomicUsize, Ordering};

use hashbrown::HashMap;
use ida::IdAllocator;
use system_error::SystemError;

use crate::{
    filesystem::vfs::syscall::ModeType,
    libs::{align::page_align_up, spinlock::SpinLock},
    mm::{
        allocator::page_frame::{FrameAllocator, PageFrameCount},
        syscall::ProtFlags,
        ucontext::AddressSpace,
        PhysAddr,
    },
    process::{Pid, ProcessManager},
    time::TimeSpec,
};

lazy_static! {
    /// 全局共享内存管理器
    pub static ref SHM_MANAGER: SpinLock<ShmManager> = SpinLock::new(ShmManager::new());
}

/// 用于创建新的私有IPC对象
pub const IPC_PRIVATE: ShmKey = ShmKey::new(0);

/// 共享内存信息
pub struct ShmData;

impl ShmData {
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
}

/// 用于接收SYS_SHMCTL系统调用传入的cmd参数
#[derive(Eq, Clone, Copy)]
pub enum ShmCtlCmd {
    /// 删除共享内存段
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

impl PartialEq for ShmCtlCmd {
    fn eq(&self, other: &ShmCtlCmd) -> bool {
        *self as usize == *other as usize
    }
}

bitflags! {
    pub struct ShmFlags:u32{
        const SHM_RDONLY = 0o10000;
        const SHM_RND = 0o20000;
        const SHM_REMAP = 0o40000;
        const SHM_EXEC = 0o100000;
        const SHM_HUGETLB = 0o4000;

        const IPC_CREAT = 0o1000;
        const IPC_EXCL = 0o2000;

        const SHM_DEST = 0o1000;
        const SHM_LOCKED = 02000;
    }
}

impl Into<ProtFlags> for ShmFlags {
    fn into(self) -> ProtFlags {
        let mut prot_flags = ProtFlags::PROT_NONE;

        if self.contains(ShmFlags::SHM_RDONLY) {
            prot_flags.insert(ProtFlags::PROT_READ);
        } else {
            prot_flags.insert(ProtFlags::PROT_READ);
            prot_flags.insert(ProtFlags::PROT_WRITE);
        }

        if self.contains(ShmFlags::SHM_EXEC) {
            prot_flags.insert(ProtFlags::PROT_EXEC);
        }

        return prot_flags;
    }
}

/// 共享内存管理器
#[derive(Debug)]
pub struct ShmManager {
    /// id分配器
    id_alloator: IdAllocator,
    /// 共享内存表
    id2shm: HashMap<ShmId, SpinLock<ShmKernel>>,
    /// id表
    key2id: HashMap<ShmKey, ShmId>,
    /// 共享内存元信息
    metadata: ShmMetaInfo,
}

impl ShmManager {
    pub fn new() -> Self {
        ShmManager {
            id_alloator: IdAllocator::new(0, usize::MAX - 1),
            id2shm: HashMap::new(),
            key2id: HashMap::new(),
            metadata: ShmMetaInfo::new(),
        }
    }

    /// # 添加共享内存段
    ///
    /// ## 参数
    ///
    /// - `key`: 共享内存键值
    /// - `size`: 共享内存大小
    /// - `shm_flags`: 共享内存标志
    ///
    /// ## 返回值
    ///
    /// 成功：共享内存id
    /// 失败：对应错误码
    pub fn add(
        &mut self,
        key: ShmKey,
        size: usize,
        shm_flags: ShmFlags,
    ) -> Result<usize, SystemError> {
        // 判断共享内存大小是否有效
        if size < ShmData::SHMMIN || size > ShmData::SHMMAX {
            return Err(SystemError::EINVAL);
        }

        // 分配共享内存对应物理页面
        let current_address_space = AddressSpace::current()?;
        let mut binding = current_address_space.write();
        let allocator = binding.user_mapper.utable.allocator_mut();
        compiler_fence(Ordering::SeqCst);
        let phys_page =
            unsafe { allocator.allocate(PageFrameCount::from_bytes(page_align_up(size)).unwrap()) }
                .ok_or(SystemError::EINVAL);
        compiler_fence(Ordering::SeqCst);

        // 创建IPC对象
        let paddr = phys_page.unwrap().0;
        let id = self.id_alloator.alloc().expect("No more id to allocate.");
        let shm_id = ShmId::new(id);
        let shm_cprid = ProcessManager::current_pid();
        let time = TimeSpec::now();
        let shm_ctim = time.tv_sec * 1000000000 + time.tv_nsec;
        let kern_ipc_perm = KernIpcPerm {
            id: shm_id,
            key: key,
            uid: 0,
            gid: 0,
            _cuid: 0,
            _cgid: 0,
            mode: shm_flags & ShmFlags::from_bits_truncate(ModeType::S_IRWXUGO.bits()),
            _seq: 0,
        };
        let shm_kernel = ShmKernel {
            kern_ipc_perm,
            shm_nattch: AtomicUsize::new(0),
            shm_paddr: paddr,
            shm_segsz: size,
            shm_atim: 0,
            shm_dtim: 0,
            shm_ctim,
            shm_cprid: shm_cprid,
            shm_lprid: shm_cprid,
        };

        // 将key、id及其对应IPC对象添加到表中
        self.id2shm.insert(shm_id, SpinLock::new(shm_kernel));
        self.key2id.insert(key, shm_id);

        return Ok(shm_id.data());
    }

    pub fn find_key(&self, key: ShmKey) -> Option<&ShmId> {
        self.key2id.get(&key)
    }

    /// # 判断物理区域是否可以收回
    ///
    /// ## 参数
    /// - `paddr`: 物理区域起始地址
    ///
    /// ## 返回值
    /// true: 可以收回
    /// false: 不可以收回
    pub fn can_deallocate(&mut self, paddr: PhysAddr) -> bool {
        let mut id_move = ShmId::new(usize::MAX);
        let mut key_move = ShmKey::new(0);
        for (id, shm_kernel) in self.id2shm.iter() {
            let mut shm_kernel_guard = shm_kernel.lock();

            // 当前物理地址是共享内存起始地址
            if shm_kernel_guard.shm_paddr == paddr {
                // 共享内存链接数减1
                shm_kernel_guard.sub_nattch();
                // 连接数等于0且设置了SHM_DEST标志，才可以移除
                if shm_kernel_guard.nattch() == 0
                    && shm_kernel_guard.mode().contains(ShmFlags::SHM_DEST)
                {
                    id_move = id.clone();
                    key_move = shm_kernel_guard.key();
                    break;
                }
                // 链接数不等于0，仍有进程使用该共享内存段，或者没有设置SHM_DEST标志，不能回收，返回false
                else {
                    return false;
                }
            }
            // 当前物理地址属于共享内存段中间部分，不能回收
            else if paddr > shm_kernel_guard.shm_paddr
                && paddr < shm_kernel_guard.shm_paddr + shm_kernel_guard.shm_segsz
            {
                return false;
            }
        }

        // id_move值被修改过，说明需要将其从表中删去
        if id_move != ShmId::new(usize::MAX) {
            self.free(id_move, key_move);
        }

        return true;
    }

    /// 释放IPC对象、key以及分配的id
    pub fn free(&mut self, id: ShmId, key: ShmKey) {
        self.id2shm.remove(&id);
        self.key2id.remove(&key);
        self.id_alloator.free(id.data());
    }

    pub fn get(&self, id: ShmId) -> Option<&SpinLock<ShmKernel>> {
        self.id2shm.get(&id)
    }

    /// 增加链接数
    pub fn add_nattch(&mut self, id: ShmId) {
        self.id2shm.get(&id).unwrap().lock().add_nattch();
    }

    /// 获取已被使用的id数
    pub fn used_ids(&self) -> usize {
        self.id2shm.len()
    }

    /// 获取共享内存总量(pages)
    pub fn shm_tot(&self) -> usize {
        self.id2shm.iter().fold(0, |acc, (_, shm_kernel)| {
            acc + PageFrameCount::from_bytes(page_align_up(shm_kernel.lock().size()))
                .unwrap()
                .data()
        })
    }

    pub fn metadata(&self) -> ShmMetaInfo {
        self.metadata
    }
}

int_like!(ShmId, usize);

int_like!(ShmKey, usize);

/// IPC对象权限
#[derive(Debug)]
pub struct KernIpcPerm {
    /// IPC对象id
    id: ShmId,
    /// IPC对象键值，由创建共享内存用户指定
    key: ShmKey,
    /// IPC对象拥有者id
    uid: usize,
    /// IPC对象拥有者组id
    gid: usize,
    /// IPC对象创建者id
    _cuid: usize,
    /// IPC对象创建者组id
    _cgid: usize,
    /// 共享内存区权限
    mode: ShmFlags,
    _seq: usize,
}

/// IPC对象
#[derive(Debug)]
pub struct ShmKernel {
    /// IPC对象权限
    kern_ipc_perm: KernIpcPerm,
    /// 链接数
    shm_nattch: AtomicUsize,
    /// 共享内存起始物理地址
    shm_paddr: PhysAddr,
    /// 共享内存大小(bytes)
    shm_segsz: usize,
    /// 最后一次连接的时间
    shm_atim: i64,
    /// 最后一次断开连接的时间
    shm_dtim: i64,
    /// 最后一次更改信息的时间
    shm_ctim: i64,
    /// 创建者进程id
    shm_cprid: Pid,
    /// 最后操作者进程id
    shm_lprid: Pid,
}

impl ShmKernel {
    pub fn id(&self) -> ShmId {
        self.kern_ipc_perm.id
    }

    pub fn key(&self) -> ShmKey {
        self.kern_ipc_perm.key
    }

    pub fn size(&self) -> usize {
        self.shm_segsz
    }

    pub fn mode(&self) -> ShmFlags {
        self.kern_ipc_perm.mode
    }

    pub fn paddr(&self) -> PhysAddr {
        self.shm_paddr
    }

    pub fn nattch(&self) -> usize {
        self.shm_nattch.load(Ordering::Relaxed)
    }

    pub fn atim(&self) -> i64 {
        self.shm_atim
    }

    pub fn dtim(&self) -> i64 {
        self.shm_dtim
    }

    pub fn ctim(&self) -> i64 {
        self.shm_ctim
    }

    pub fn cprid(&self) -> Pid {
        self.shm_cprid
    }

    pub fn lprid(&self) -> Pid {
        self.shm_lprid
    }

    /// 增加链接数
    pub fn add_nattch(&mut self) {
        self.shm_nattch.fetch_add(1, Ordering::Relaxed);
        self.update_atim();
        self.update_lprid();
    }

    /// 减少链接数
    pub fn sub_nattch(&mut self) {
        self.shm_nattch.fetch_sub(1, Ordering::Relaxed);
        self.update_dtim();
        self.update_lprid();
    }

    /// 更新最后一次连接时间
    pub fn update_atim(&mut self) {
        let time = TimeSpec::now();
        self.shm_atim = time.tv_sec * 1000000000 + time.tv_nsec;
    }

    /// 更新最后一次断开连接时间
    pub fn update_dtim(&mut self) {
        let time = TimeSpec::now();
        self.shm_dtim = time.tv_sec * 1000000000 + time.tv_nsec;
    }

    /// 更新最后一次修改信息时间
    pub fn update_ctim(&mut self) {
        let time = TimeSpec::now();
        self.shm_ctim = time.tv_sec * 1000000000 + time.tv_nsec;
    }

    /// 更新最后一个操作共享内存的进程id
    pub fn update_lprid(&mut self) {
        self.shm_lprid = ProcessManager::current_pid();
    }

    pub fn copy_from_ipc_perm(&mut self, shm_id_ds: &ShmIdDs) {
        self.kern_ipc_perm.uid = shm_id_ds.uid() as usize;
        self.kern_ipc_perm.gid = shm_id_ds.gid() as usize;
        self.kern_ipc_perm.mode = ShmFlags::from_bits_truncate(shm_id_ds.mode());
        self.update_ctim();
    }

    /// 设置IPC权限，增加SHM_LOCKED标志，表示该共享内存无法被置换出内存
    pub fn lock(&mut self) {
        self.kern_ipc_perm.mode.insert(ShmFlags::SHM_LOCKED);
        self.update_ctim();
    }

    /// 设置IPC权限，取消SHM_LOCKED标志
    pub fn unlock(&mut self) {
        self.kern_ipc_perm.mode.remove(ShmFlags::SHM_LOCKED);
        self.update_ctim();
    }

    /// 设置IPC权限，增加SHM_DEST标志，表示当最后一个进程取消连接后，释放该共享内存
    pub fn set_dest(&mut self) {
        self.kern_ipc_perm.mode.insert(ShmFlags::SHM_DEST);
        self.update_ctim();
    }

    pub fn debug(&self) {
        kdebug!("{:?}", self);
    }
}

/// 共享内存元信息
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ShmMetaInfo {
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

impl ShmMetaInfo {
    pub fn new() -> Self {
        ShmMetaInfo {
            shmmax: ShmData::SHMMAX,
            shmmin: ShmData::SHMMIN,
            shmmni: ShmData::SHMMNI,
            shmseg: ShmData::SHMSEG,
            shmall: ShmData::SHMALL,
            _unused1: 0,
            _unused2: 0,
            _unused3: 0,
            _unused4: 0,
        }
    }
}

/// 共享内存信息
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ShmInfo {
    /// 已使用id数
    used_ids: i32,
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

impl ShmInfo {
    pub fn new(
        used_ids: i32,
        shm_tot: usize,
        shm_rss: usize,
        shm_swp: usize,
        swap_attempts: usize,
        swap_successes: usize,
    ) -> Self {
        ShmInfo {
            used_ids,
            shm_tot,
            shm_rss,
            shm_swp,
            swap_attempts,
            swap_successes,
        }
    }
}

/// 共享内存段属性信息
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ShmIdDs {
    /// 共享内存段权限
    shm_perm: IpcPerm,
    /// 共享内存大小(bytes)
    shm_segsz: usize,
    /// 最后一次连接的时间
    shm_atime: i64,
    /// 最后一次断开连接的时间
    shm_dtime: i64,
    /// 最后一次更改信息的时间
    shm_ctime: i64,
    /// 创建者进程id
    shm_cpid: u32,
    /// 最后操作者进程id
    shm_lpid: u32,
    /// 链接数
    shm_nattch: usize,
    _unused1: usize,
    _unused2: usize,
}

impl ShmIdDs {
    pub fn new(
        shm_perm: IpcPerm,
        shm_segsz: usize,
        shm_atime: i64,
        shm_dtime: i64,
        shm_ctime: i64,
        shm_cpid: u32,
        shm_lpid: u32,
        shm_nattch: usize,
    ) -> Self {
        ShmIdDs {
            shm_perm,
            shm_segsz,
            shm_atime,
            shm_dtime,
            shm_ctime,
            shm_cpid,
            shm_lpid,
            shm_nattch,
            _unused1: 0,
            _unused2: 0,
        }
    }

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

/// 共享内存段权限
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct IpcPerm {
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

impl IpcPerm {
    pub fn new(key: i32, uid: u32, gid: u32, cuid: u32, cgid: u32, mode: u32) -> Self {
        IpcPerm {
            key,
            uid,
            gid,
            cuid,
            cgid,
            mode,
            seq: 0,
            _pad1: 0,
            _unused1: 0,
            _unused2: 0,
        }
    }
}
