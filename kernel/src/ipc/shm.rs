use crate::{
    arch::mm::LockedFrameAllocator,
    filesystem::vfs::syscall::ModeType,
    libs::{
        align::page_align_up,
        spinlock::{SpinLock, SpinLockGuard},
    },
    mm::{
        allocator::page_frame::{FrameAllocator, PageFrameCount, PhysPageFrame},
        page::{page_manager_lock_irqsave, PageFlags, PageType},
        PhysAddr,
    },
    process::{Pid, ProcessManager},
    syscall::user_access::{UserBufferReader, UserBufferWriter},
    time::PosixTimeSpec,
};
use core::sync::atomic::{compiler_fence, Ordering};
use hashbrown::HashMap;
use ida::IdAllocator;
use log::info;
use num::ToPrimitive;
use system_error::SystemError;

pub static mut SHM_MANAGER: Option<SpinLock<ShmManager>> = None;

/// 用于创建新的私有IPC对象
pub const IPC_PRIVATE: ShmKey = ShmKey::new(0);

/// 初始化SHM_MANAGER
pub fn shm_manager_init() {
    info!("shm_manager_init");
    let shm_manager = SpinLock::new(ShmManager::new());

    compiler_fence(Ordering::SeqCst);
    unsafe { SHM_MANAGER = Some(shm_manager) };
    compiler_fence(Ordering::SeqCst);

    info!("shm_manager_init done");
}

pub fn shm_manager_lock() -> SpinLockGuard<'static, ShmManager> {
    unsafe { SHM_MANAGER.as_ref().unwrap().lock() }
}

int_like!(ShmId, usize);
int_like!(ShmKey, usize);

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
        const SHM_LOCKED = 0o2000;
    }
}

/// 管理共享内存段信息的操作码
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

/// 共享内存管理器
#[derive(Debug)]
pub struct ShmManager {
    /// ShmId分配器
    id_allocator: IdAllocator,
    /// ShmId映射共享内存信息表
    id2shm: HashMap<ShmId, KernelShm>,
    /// ShmKey映射ShmId表
    key2id: HashMap<ShmKey, ShmId>,
}

impl ShmManager {
    pub fn new() -> Self {
        ShmManager {
            id_allocator: IdAllocator::new(0, usize::MAX - 1).unwrap(),
            id2shm: HashMap::new(),
            key2id: HashMap::new(),
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
    pub fn add(
        &mut self,
        key: ShmKey,
        size: usize,
        shmflg: ShmFlags,
    ) -> Result<usize, SystemError> {
        // 判断共享内存大小是否过小或溢出
        if !(PosixShmMetaInfo::SHMMIN..=PosixShmMetaInfo::SHMMAX).contains(&size) {
            return Err(SystemError::EINVAL);
        }

        let id = self.id_allocator.alloc().expect("No more id to allocate.");
        let shm_id = ShmId::new(id);

        // 分配共享内存页面
        let page_count = PageFrameCount::from_bytes(page_align_up(size)).unwrap();
        // 创建共享内存page，并添加到PAGE_MANAGER中
        let mut page_manager_guard = page_manager_lock_irqsave();
        let (paddr, _page) = page_manager_guard.create_pages(
            PageType::Shm(shm_id),
            PageFlags::PG_UNEVICTABLE,
            &mut LockedFrameAllocator,
            page_count,
        )?;

        // 创建共享内存信息结构体
        let kern_ipc_perm = KernIpcPerm {
            id: shm_id,
            key,
            uid: 0,
            gid: 0,
            _cuid: 0,
            _cgid: 0,
            mode: shmflg & ShmFlags::from_bits_truncate(ModeType::S_IRWXUGO.bits()),
            _seq: 0,
        };
        let shm_kernel = KernelShm::new(kern_ipc_perm, paddr, size);

        // 将key、id及其对应KernelShm添加到表中
        self.id2shm.insert(shm_id, shm_kernel);
        self.key2id.insert(key, shm_id);

        return Ok(shm_id.data());
    }

    pub fn contains_key(&self, key: &ShmKey) -> Option<&ShmId> {
        self.key2id.get(key)
    }

    pub fn get_mut(&mut self, id: &ShmId) -> Option<&mut KernelShm> {
        self.id2shm.get_mut(id)
    }

    pub fn free_key(&mut self, key: &ShmKey) {
        self.key2id.remove(key);
    }

    pub fn free_id(&mut self, id: &ShmId) {
        self.id2shm.remove(id);
        self.id_allocator.free(id.0);
    }

    pub fn ipc_info(&self, user_buf: *const u8, from_user: bool) -> Result<usize, SystemError> {
        let mut user_buffer_writer = UserBufferWriter::new(
            user_buf as *mut u8,
            core::mem::size_of::<PosixShmMetaInfo>(),
            from_user,
        )?;

        let shm_meta_info = PosixShmMetaInfo::new();
        user_buffer_writer.copy_one_to_user(&shm_meta_info, 0)?;

        return Ok(0);
    }

    pub fn shm_info(&self, user_buf: *const u8, from_user: bool) -> Result<usize, SystemError> {
        // 已使用id数量
        let used_ids = self.id2shm.len().to_i32().unwrap();
        // 共享内存总和
        let shm_tot = self.id2shm.iter().fold(0, |acc, (_, kernel_shm)| {
            acc + PageFrameCount::from_bytes(page_align_up(kernel_shm.shm_size))
                .unwrap()
                .data()
        });
        let shm_info = PosixShmInfo::new(used_ids, shm_tot, 0, 0, 0, 0);

        let mut user_buffer_writer = UserBufferWriter::new(
            user_buf as *mut u8,
            core::mem::size_of::<PosixShmInfo>(),
            from_user,
        )?;
        user_buffer_writer.copy_one_to_user(&shm_info, 0)?;

        return Ok(0);
    }

    pub fn shm_stat(
        &self,
        id: ShmId,
        cmd: ShmCtlCmd,
        user_buf: *const u8,
        from_user: bool,
    ) -> Result<usize, SystemError> {
        let kernel_shm = self.id2shm.get(&id).ok_or(SystemError::EINVAL)?;
        let key = kernel_shm.kern_ipc_perm.key.data().to_i32().unwrap();
        let mode = kernel_shm.kern_ipc_perm.mode.bits();

        let shm_perm = PosixIpcPerm::new(key, 0, 0, 0, 0, mode);
        let shm_segsz = kernel_shm.shm_size;
        let shm_atime = kernel_shm.shm_atim.total_nanos();
        let shm_dtime = kernel_shm.shm_dtim.total_nanos();
        let shm_ctime = kernel_shm.shm_ctim.total_nanos();
        let shm_cpid = kernel_shm.shm_cprid.data().to_u32().unwrap();
        let shm_lpid = kernel_shm.shm_lprid.data().to_u32().unwrap();
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

        let mut user_buffer_writer = UserBufferWriter::new(
            user_buf as *mut u8,
            core::mem::size_of::<PosixShmIdDs>(),
            from_user,
        )?;
        user_buffer_writer.copy_one_to_user(&shm_id_ds, 0)?;

        let r: usize = if cmd == ShmCtlCmd::IpcStat {
            0
        } else {
            id.data()
        };

        return Ok(r);
    }

    pub fn ipc_set(
        &mut self,
        id: ShmId,
        user_buf: *const u8,
        from_user: bool,
    ) -> Result<usize, SystemError> {
        let kernel_shm = self.id2shm.get_mut(&id).ok_or(SystemError::EINVAL)?;

        let user_buffer_reader =
            UserBufferReader::new(user_buf, core::mem::size_of::<PosixShmIdDs>(), from_user)?;
        let mut shm_id_ds = PosixShmIdDs::default();
        user_buffer_reader.copy_one_from_user(&mut shm_id_ds, 0)?;

        kernel_shm.copy_from(shm_id_ds);

        return Ok(0);
    }

    pub fn ipc_rmid(&mut self, id: ShmId) -> Result<usize, SystemError> {
        let kernel_shm = self.id2shm.get_mut(&id).ok_or(SystemError::EINVAL)?;
        kernel_shm.set_mode(ShmFlags::SHM_DEST, true);

        let mut cur_phys = PhysPageFrame::new(kernel_shm.shm_start_paddr);
        let count = PageFrameCount::from_bytes(page_align_up(kernel_shm.shm_size)).unwrap();
        let key = kernel_shm.kern_ipc_perm.key;
        let id = kernel_shm.kern_ipc_perm.id;
        let map_count = kernel_shm.map_count();

        let mut page_manager_guard = page_manager_lock_irqsave();
        if map_count > 0 {
            // 设置共享内存物理页当映射计数等于0时可被回收
            // TODO 后续需要加入到lru中
            for _ in 0..count.data() {
                let page = page_manager_guard.get_unwrap(&cur_phys.phys_address());
                page.write_irqsave().remove_flags(PageFlags::PG_UNEVICTABLE);

                cur_phys = cur_phys.next();
            }

            // 释放key，不让后续进程连接
            self.free_key(&key);
        } else {
            // 释放共享内存物理页
            for _ in 0..count.data() {
                let paddr = cur_phys.phys_address();
                unsafe {
                    LockedFrameAllocator.free(paddr, PageFrameCount::new(1));
                }
                // 将已回收的物理页面对应的Page从PAGE_MANAGER中删去
                page_manager_guard.remove_page(&paddr);
                cur_phys = cur_phys.next();
            }

            // 释放key和id
            self.free_id(&id);
            self.free_key(&key)
        }

        return Ok(0);
    }

    pub fn shm_lock(&mut self, id: ShmId) -> Result<usize, SystemError> {
        let kernel_shm = self.id2shm.get_mut(&id).ok_or(SystemError::EINVAL)?;
        kernel_shm.set_mode(ShmFlags::SHM_LOCKED, true);

        return Ok(0);
    }

    pub fn shm_unlock(&mut self, id: ShmId) -> Result<usize, SystemError> {
        let kernel_shm = self.id2shm.get_mut(&id).ok_or(SystemError::EINVAL)?;
        kernel_shm.set_mode(ShmFlags::SHM_LOCKED, false);

        return Ok(0);
    }
}
/// 共享内存信息
#[derive(Debug)]
pub struct KernelShm {
    /// 权限信息
    kern_ipc_perm: KernIpcPerm,
    /// 共享内存起始物理地址
    shm_start_paddr: PhysAddr,
    /// 共享内存大小(bytes)，注意是用户指定的大小（未经过页面对齐）
    shm_size: usize,
    /// 映射计数
    map_count: usize,
    /// 最后一次连接的时间
    shm_atim: PosixTimeSpec,
    /// 最后一次断开连接的时间
    shm_dtim: PosixTimeSpec,
    /// 最后一次更改信息的时间
    shm_ctim: PosixTimeSpec,
    /// 创建者进程id
    shm_cprid: Pid,
    /// 最后操作者进程id
    shm_lprid: Pid,
}

impl KernelShm {
    pub fn new(kern_ipc_perm: KernIpcPerm, shm_start_paddr: PhysAddr, shm_size: usize) -> Self {
        let shm_cprid = ProcessManager::current_pid();
        KernelShm {
            kern_ipc_perm,
            shm_start_paddr,
            shm_size,
            map_count: 0,
            shm_atim: PosixTimeSpec::new(0, 0),
            shm_dtim: PosixTimeSpec::new(0, 0),
            shm_ctim: PosixTimeSpec::now(),
            shm_cprid,
            shm_lprid: shm_cprid,
        }
    }

    pub fn start_paddr(&self) -> PhysAddr {
        self.shm_start_paddr
    }

    pub fn size(&self) -> usize {
        self.shm_size
    }

    /// 更新最后连接时间
    pub fn update_atim(&mut self) {
        // 更新最后一次连接时间
        self.shm_atim = PosixTimeSpec::now();

        // 更新最后操作当前共享内存的进程ID
        self.shm_lprid = ProcessManager::current_pid();
    }

    /// 更新最后断开连接时间
    pub fn update_dtim(&mut self) {
        // 更新最后一次断开连接时间
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
        self.map_count
    }

    pub fn copy_from(&mut self, shm_id_ds: PosixShmIdDs) {
        self.kern_ipc_perm.uid = shm_id_ds.uid() as usize;
        self.kern_ipc_perm.gid = shm_id_ds.gid() as usize;
        self.kern_ipc_perm.mode = ShmFlags::from_bits_truncate(shm_id_ds.mode());
        self.update_ctim();
    }

    pub fn set_mode(&mut self, shmflg: ShmFlags, set: bool) {
        if set {
            self.kern_ipc_perm.mode.insert(shmflg);
        } else {
            self.kern_ipc_perm.mode.remove(shmflg);
        }

        self.update_ctim();
    }

    pub fn mode(&self) -> &ShmFlags {
        &self.kern_ipc_perm.mode
    }

    pub fn increase_count(&mut self) {
        self.map_count += 1;
    }

    pub fn decrease_count(&mut self) {
        assert!(self.map_count > 0, "map_count is zero");
        self.map_count -= 1;
    }
}

/// 共享内存权限信息
#[derive(Debug)]
pub struct KernIpcPerm {
    /// 共享内存id
    id: ShmId,
    /// 共享内存键值，由创建共享内存用户指定
    key: ShmKey,
    /// 共享内存拥有者用户id
    uid: usize,
    /// 共享内存拥有者所在组id
    gid: usize,
    /// 共享内存创建者用户id
    _cuid: usize,
    /// 共享内存创建者所在组id
    _cgid: usize,
    /// 共享内存区权限模式
    mode: ShmFlags,
    _seq: usize,
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

impl PosixIpcPerm {
    pub fn new(key: i32, uid: u32, gid: u32, cuid: u32, cgid: u32, mode: u32) -> Self {
        PosixIpcPerm {
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
