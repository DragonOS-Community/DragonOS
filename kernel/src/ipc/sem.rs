// SPDX-License-Identifier: GPL-2.0-or-later
//
// System V semaphore subsystem, tracking Linux 6.6 `ipc/sem.c` observable
// behavior. SEM_UNDO is intentionally unsupported and rejected with ENOSYS.

use alloc::{collections::VecDeque, sync::Arc, vec::Vec};
use core::fmt;

use hashbrown::HashMap;
use system_error::SystemError;

use crate::{
    ipc::{
        id::IpcIdAllocator,
        ipc_perm::{self, IpcPerm, IpcPermView, PosixIpcPerm},
    },
    libs::wait_queue::{TimeoutWaker, Waiter, Waker},
    process::{namespace::ipc_namespace::IpcNamespace, ProcessManager, RawPid},
    time::{
        timer::{clock, next_n_us_timer_jiffies, Timer},
        Duration, PosixTimeSpec,
    },
};

/// 用于创建新的私有信号量集合
pub const IPC_PRIVATE: SemKey = SemKey::new(0);

int_like!(SemId, usize);
int_like!(SemKey, usize);

// Linux include/uapi/linux/sem.h 的限制常量
pub const SEMMNI: usize = 32000;
pub const SEMMSL: usize = 32000;
pub const SEMMNS: usize = SEMMNI * SEMMSL;
pub const SEMOPM: usize = 500;
pub const SEMVMX: i32 = 32767;
pub const SEMAEM: usize = 16384;
const SEMUME: usize = 32;
const SEMUSZ: usize = 128;

bitflags! {
    pub struct SemFlags: u32 {
        const PERM_MASK = 0o777;
        const IPC_CREAT = 0o1000;
        const IPC_EXCL = 0o2000;
        const IPC_NOWAIT = 0x800;
        const SEM_UNDO = 0x1000;
    }
}

/// 管理信号量集合信息的操作码（Linux x86_64 UAPI include/uapi/linux/sem.h）
#[derive(Eq, Clone, Copy)]
pub enum SemCtlCmd {
    /// 删除信号量集合
    IpcRmid = 0,
    /// 设置权限
    IpcSet = 1,
    /// 获取 SemIdDs
    IpcStat = 2,
    /// 查看 SemInfo
    IpcInfo = 3,
    /// 获取最后操作指定信号量的进程pid
    GetPid = 11,
    /// 获取指定信号量的值
    GetVal = 12,
    /// 获取集合内所有信号量的值
    GetAll = 13,
    /// 获取等待指定信号量值增加的进程数
    GetNcnt = 14,
    /// 获取等待指定信号量值为0的进程数
    GetZcnt = 15,
    /// 设置指定信号量的值
    SetVal = 16,
    /// 设置集合内所有信号量的值
    SetAll = 17,
    /// 按索引获取 SemIdDs
    SemStat = 18,
    /// 查看 SemInfo
    SemInfo = 19,
    /// 按索引获取 SemIdDs（无权限检查）
    SemStatAny = 20,

    Default,
}

impl From<usize> for SemCtlCmd {
    fn from(cmd: usize) -> SemCtlCmd {
        match cmd {
            0 => Self::IpcRmid,
            1 => Self::IpcSet,
            2 => Self::IpcStat,
            3 => Self::IpcInfo,
            11 => Self::GetPid,
            12 => Self::GetVal,
            13 => Self::GetAll,
            14 => Self::GetNcnt,
            15 => Self::GetZcnt,
            16 => Self::SetVal,
            17 => Self::SetAll,
            18 => Self::SemStat,
            19 => Self::SemInfo,
            20 => Self::SemStatAny,
            _ => Self::Default,
        }
    }
}

impl fmt::Display for SemCtlCmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SemCtlCmd::IpcRmid => write!(f, "IPC_RMID"),
            SemCtlCmd::IpcSet => write!(f, "IPC_SET"),
            SemCtlCmd::IpcStat => write!(f, "IPC_STAT"),
            SemCtlCmd::IpcInfo => write!(f, "IPC_INFO"),
            SemCtlCmd::GetPid => write!(f, "GETPID"),
            SemCtlCmd::GetVal => write!(f, "GETVAL"),
            SemCtlCmd::GetAll => write!(f, "GETALL"),
            SemCtlCmd::GetNcnt => write!(f, "GETNCNT"),
            SemCtlCmd::GetZcnt => write!(f, "GETZCNT"),
            SemCtlCmd::SetVal => write!(f, "SETVAL"),
            SemCtlCmd::SetAll => write!(f, "SETALL"),
            SemCtlCmd::SemStat => write!(f, "SEM_STAT"),
            SemCtlCmd::SemInfo => write!(f, "SEM_INFO"),
            SemCtlCmd::SemStatAny => write!(f, "SEM_STAT_ANY"),
            SemCtlCmd::Default => write!(f, "DEFAULT (Invalid Cmd)"),
        }
    }
}

impl PartialEq for SemCtlCmd {
    fn eq(&self, other: &SemCtlCmd) -> bool {
        *self as usize == *other as usize
    }
}

/// 单个信号量（Linux struct sem 的字段）
#[derive(Debug, Clone, Copy)]
pub struct KernelSem {
    /// semval
    val: i32,
    /// sempid：最后操作本信号量的进程
    pid: RawPid,
}

/// 用户态 sembuf（Linux struct sembuf，6 字节）
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PosixSemBuf {
    pub sem_num: u16,
    pub sem_op: i16,
    pub sem_flg: i16,
}

/// 信号量集合信息，符合 Linux x86_64 struct semid64_ds（104 字节，带
/// 32 位时间戳高半区字段）
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct PosixSemIdDs {
    /// 权限信息
    pub sem_perm: PosixIpcPerm,
    /// 最后一次 semop 的时间（64 位；高 32 位落入 __sem_otime_high）
    pub sem_otime: i64,
    _sem_otime_high: i64,
    /// 最后一次更改信息的时间
    pub sem_ctime: i64,
    _sem_ctime_high: i64,
    /// 集合内信号量数量
    pub sem_nsems: usize,
    _unused1: usize,
    _unused2: usize,
}

/// 信号量系统信息，符合 Linux struct seminfo（40 字节）
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct PosixSemInfo {
    pub semmap: i32,
    pub semmni: i32,
    pub semmns: i32,
    pub semmnu: i32,
    pub semmsl: i32,
    pub semopm: i32,
    pub semume: i32,
    pub semusz: i32,
    pub semvmx: i32,
    pub semaem: i32,
}

impl PosixSemInfo {
    pub fn new() -> Self {
        PosixSemInfo {
            semmap: SEMMSL as i32,
            semmni: SEMMNI as i32,
            semmns: SEMMNS as i32,
            semmnu: SEMMNS as i32,
            semmsl: SEMMSL as i32,
            semopm: SEMOPM as i32,
            semume: SEMUME as i32,
            semusz: SEMUSZ as i32,
            semvmx: SEMVMX,
            semaem: SEMAEM as i32,
        }
    }
}

/// 等待者阻塞的原因，决定唤醒时机与 GETNCNT/GETZCNT 统计
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SemWaitType {
    /// sem_op < 0：等待 semval 增加（GETNCNT）
    Increase,
    /// sem_op == 0：等待 semval 变为 0（GETZCNT）
    Zero,
}

/// 等待队列项（Linux struct sem_queue 的简化：等待者唤醒后重新执行 sops，
/// 因此不需要保存 sops 副本）
#[derive(Debug)]
struct SemWaiter {
    semnum: usize,
    wait_type: SemWaitType,
    waker: Arc<Waker>,
}

/// 信号量集合
#[derive(Debug)]
pub struct KernelSemSet {
    /// 权限信息
    pub kern_ipc_perm: IpcPerm,
    /// 集合内信号量
    pub sems: Vec<KernelSem>,
    /// 最后一次 semop 的时间
    pub sem_otime: i64,
    /// 最后一次更改信息的时间
    pub sem_ctime: i64,
    /// 等待者队列
    waiters: VecDeque<SemWaiter>,
}

impl KernelSemSet {
    pub fn new(kern_ipc_perm: IpcPerm, nsems: usize) -> Self {
        KernelSemSet {
            kern_ipc_perm,
            sems: core::iter::repeat(KernelSem {
                val: 0,
                pid: RawPid::new(0),
            })
            .take(nsems)
            .collect(),
            sem_otime: 0,
            sem_ctime: PosixTimeSpec::now().tv_sec,
            waiters: VecDeque::new(),
        }
    }

    fn enqueue_waiter(&mut self, waiter: SemWaiter) {
        self.waiters.push_back(waiter);
    }

    fn remove_waiter(&mut self, waker: &Arc<Waker>) {
        self.waiters.retain(|w| !Arc::ptr_eq(&w.waker, waker));
    }

    /// 唤醒满足条件的等待者，其余保留。必须在持有 manager 锁时调用。
    ///
    /// 与 Linux `update_queue` 的差异：Linux 只唤醒"现在可执行"的等待者并
    /// 直接应用其操作；这里按变化方向（Increase/Zero）唤醒所有可能满足的
    /// 等待者，由等待者醒来后持锁重查。正确性等价（唤醒与重查都在锁内，
    /// 不会丢失唤醒），代价是额外的惊群唤醒与重试。
    fn wake_waiters(&mut self, changes: &[SemChange]) {
        if self.waiters.is_empty() {
            return;
        }
        let mut remain = VecDeque::new();
        for waiter in self.waiters.drain(..) {
            let satisfied = changes.iter().any(|change| match change {
                SemChange::Increase(idx) => {
                    *idx == waiter.semnum && waiter.wait_type == SemWaitType::Increase
                }
                SemChange::Zero(idx) => {
                    *idx == waiter.semnum && waiter.wait_type == SemWaitType::Zero
                }
            });
            if satisfied {
                waiter.waker.wake();
            } else {
                remain.push_back(waiter);
            }
        }
        self.waiters = remain;
    }

    /// 唤醒并清空所有等待者（IPC_RMID 时）
    fn wake_all(&mut self) {
        for waiter in self.waiters.drain(..) {
            waiter.waker.wake();
        }
    }

    fn ncnt(&self, semnum: usize) -> usize {
        self.waiters
            .iter()
            .filter(|w| w.semnum == semnum && w.wait_type == SemWaitType::Increase)
            .count()
    }

    fn zcnt(&self, semnum: usize) -> usize {
        self.waiters
            .iter()
            .filter(|w| w.semnum == semnum && w.wait_type == SemWaitType::Zero)
            .count()
    }
}

/// 信号量值变化事件，用于唤醒决策
#[derive(Debug, Clone, Copy)]
enum SemChange {
    /// semval 增加
    Increase(usize),
    /// semval 变为 0
    Zero(usize),
}

/// semop 尝试执行的结果
#[derive(Debug)]
enum SemopOutcome {
    /// 全部操作已应用
    Done,
    /// 在第 semnum 个信号量上阻塞（已回滚已应用的操作）
    Blocked {
        semnum: usize,
        wait_type: SemWaitType,
    },
}

/// 信号量管理器
#[derive(Debug)]
pub struct SemManager {
    /// SemId 分配器
    id_allocator: IpcIdAllocator,
    /// 低位 IPC idx 映射信号量集合表
    id2sem: HashMap<usize, KernelSemSet>,
    /// SemKey 映射 SemId 表
    key2id: HashMap<SemKey, SemId>,
    /// namespace 内信号量总数（Linux semmns 会计）
    total_sems: usize,
}

impl Default for SemManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SemManager {
    const IPC_READ: u32 = 0o4;
    const IPC_WRITE: u32 = 0o2;

    pub fn new() -> Self {
        SemManager {
            id_allocator: IpcIdAllocator::new(SEMMNI).unwrap(),
            id2sem: HashMap::new(),
            key2id: HashMap::new(),
            total_sems: 0,
        }
    }

    fn get_by_semid_checked(&self, id: SemId) -> Result<&KernelSemSet, SystemError> {
        let decoded = IpcIdAllocator::decode(id.data())?;
        let set = self.id2sem.get(&decoded.idx).ok_or(SystemError::EINVAL)?;
        if set.kern_ipc_perm.id != id.data() || set.kern_ipc_perm.seq != decoded.seq {
            return Err(SystemError::EINVAL);
        }
        Ok(set)
    }

    fn get_by_semid_checked_mut(&mut self, id: SemId) -> Result<&mut KernelSemSet, SystemError> {
        let decoded = IpcIdAllocator::decode(id.data())?;
        let set = self
            .id2sem
            .get_mut(&decoded.idx)
            .ok_or(SystemError::EINVAL)?;
        if set.kern_ipc_perm.id != id.data() || set.kern_ipc_perm.seq != decoded.seq {
            return Err(SystemError::EINVAL);
        }
        Ok(set)
    }

    fn get_by_index(&self, idx: usize) -> Result<&KernelSemSet, SystemError> {
        if idx > IpcIdAllocator::IPC_ID_IDX_MASK {
            return Err(SystemError::EINVAL);
        }
        self.id2sem.get(&idx).ok_or(SystemError::EINVAL)
    }

    fn current_max_index(&self) -> usize {
        self.id2sem.keys().copied().max().unwrap_or(0)
    }

    /// # semget：创建或查找信号量集合
    pub fn semget(
        &mut self,
        key: SemKey,
        nsems: usize,
        semflg: SemFlags,
    ) -> Result<usize, SystemError> {
        if nsems == 0 || nsems > SEMMSL {
            return Err(SystemError::EINVAL);
        }

        if key == IPC_PRIVATE {
            return self.create(key, nsems, semflg);
        }

        if let Some(&id) = self.key2id.get(&key) {
            if semflg.contains(SemFlags::IPC_CREAT | SemFlags::IPC_EXCL) {
                return Err(SystemError::EEXIST);
            }
            let set = self.get_by_semid_checked(id)?;
            if nsems > set.sems.len() {
                return Err(SystemError::EINVAL);
            }
            let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
            ipc_perm::ipc_permission(
                &set.kern_ipc_perm,
                semflg.bits() & SemFlags::PERM_MASK.bits(),
                &target_user_ns,
            )?;
            return Ok(id.data());
        }

        if !semflg.contains(SemFlags::IPC_CREAT) {
            return Err(SystemError::ENOENT);
        }
        self.create(key, nsems, semflg)
    }

    fn create(&mut self, key: SemKey, nsems: usize, semflg: SemFlags) -> Result<usize, SystemError> {
        if self.id2sem.len() >= SEMMNI {
            return Err(SystemError::ENOSPC);
        }
        let total_after = self
            .total_sems
            .checked_add(nsems)
            .ok_or(SystemError::ENOSPC)?;
        if total_after > SEMMNS {
            return Err(SystemError::ENOSPC);
        }

        let ipc_id = self.id_allocator.alloc()?;
        let sem_id = SemId::new(ipc_id.raw);
        let current_cred = ProcessManager::current_pcb().cred();
        let kern_ipc_perm = IpcPerm::new_with_cred(
            sem_id.data(),
            key.data(),
            current_cred,
            semflg.bits() & SemFlags::PERM_MASK.bits(),
            ipc_id.seq,
        );
        let set = KernelSemSet::new(kern_ipc_perm, nsems);

        if key != IPC_PRIVATE {
            self.key2id.insert(key, sem_id);
        }
        self.id2sem.insert(ipc_id.idx, set);
        self.total_sems = total_after;

        Ok(sem_id.data())
    }

    /// 尝试执行 sops。若某个操作无法执行，回滚已应用的操作并返回阻塞位置。
    /// 必须在持有 manager 锁时调用。
    fn try_semop(
        set: &mut KernelSemSet,
        sops: &[PosixSemBuf],
    ) -> Result<SemopOutcome, SystemError> {
        for op in sops {
            let idx = op.sem_num as usize;
            if idx >= set.sems.len() {
                return Err(SystemError::EFBIG);
            }
            let sem = &set.sems[idx];
            let result = sem.val as i64 + op.sem_op as i64;
            if result > SEMVMX as i64 {
                // Linux 语义：结果超过 SEMVMX 是立即错误，不阻塞（该条件只能靠
                // val 减小满足，而减小不会唤醒任何等待者）
                return Err(SystemError::ERANGE);
            }
            if result < 0 || (op.sem_op == 0 && sem.val != 0) {
                if (op.sem_flg as u32) & SemFlags::IPC_NOWAIT.bits() != 0 {
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }
                let wait_type = if op.sem_op == 0 {
                    SemWaitType::Zero
                } else {
                    SemWaitType::Increase
                };
                return Ok(SemopOutcome::Blocked {
                    semnum: idx,
                    wait_type,
                });
            }
        }

        // 全部可执行：应用并收集变化事件
        let pid = ProcessManager::current_pid();
        let mut changes = Vec::new();
        for op in sops {
            let idx = op.sem_num as usize;
            let sem = &mut set.sems[idx];
            let old = sem.val;
            sem.val += op.sem_op as i32;
            sem.pid = pid;
            if sem.val > old {
                changes.push(SemChange::Increase(idx));
            }
            if sem.val == 0 {
                changes.push(SemChange::Zero(idx));
            }
        }
        set.sem_otime = PosixTimeSpec::now().tv_sec;
        set.wake_waiters(&changes);
        Ok(SemopOutcome::Done)
    }

    /// # semtimedop：原子执行 sops，必要时阻塞
    ///
    /// 锁由本函数内部管理（等待期间需要释放锁），调用方不得预先持有
    /// `ipcns.sem` 锁。
    ///
    /// - timeout == None：无限等待（等价于 semop）
    /// - timeout == Some(Duration::ZERO)：不阻塞
    /// - 其余：阻塞至超时，超时返回 EAGAIN
    pub fn semtimedop(
        ipcns: &Arc<IpcNamespace>,
        semid: SemId,
        sops: &[PosixSemBuf],
        timeout: Option<Duration>,
    ) -> Result<usize, SystemError> {
        if sops.is_empty() {
            return Err(SystemError::EINVAL);
        }
        if sops.len() > SEMOPM {
            return Err(SystemError::E2BIG);
        }
        if sops
            .iter()
            .any(|op| (op.sem_flg as u32) & SemFlags::SEM_UNDO.bits() != 0)
        {
            return Err(SystemError::ENOSYS);
        }
        let non_blocking = timeout == Some(Duration::ZERO);
        // 仅等待/等零（全部 sem_op == 0）时检查读权限，否则检查写权限（Linux 语义）
        let alter = sops.iter().any(|op| op.sem_op != 0);

        let target_user_ns = ipcns.user_ns.clone();
        let deadline_ticks = timeout.map(|d| {
            let ticks = next_n_us_timer_jiffies(d.total_micros());
            clock().saturating_add(ticks)
        });

        // 单个 waiter/waker 贯穿整个等待过程
        let (waiter, waker) = Waiter::new_pair();
        let timer = deadline_ticks.map(|deadline| Timer::new(TimeoutWaker::new(waker.clone()), deadline));
        let mut wait_interrupted = false;
        // 本调用是否曾成功获得集合：区分"首次调用 id 无效"（EINVAL）与
        // "等待期间被 IPC_RMID 删除"（EIDRM），与 Linux 语义一致
        let mut have_set = false;

        loop {
            let mut guard = ipcns.sem.lock();
            let set = match guard.get_by_semid_checked_mut(semid) {
                Ok(set) => set,
                Err(_) => {
                    if have_set {
                        return Err(SystemError::EIDRM);
                    }
                    return Err(SystemError::EINVAL);
                }
            };
            have_set = true;
            // 与 Linux 一致：先检查 semnum 越界（EFBIG），再检查权限（EACCES）
            if sops
                .iter()
                .any(|op| op.sem_num as usize >= set.sems.len())
            {
                return Err(SystemError::EFBIG);
            }
            ipc_perm::ipc_permission(
                &set.kern_ipc_perm,
                if alter { Self::IPC_WRITE } else { Self::IPC_READ },
                &target_user_ns,
            )?;

            // 移除可能残留的旧等待项（被超时/信号唤醒后仍在队列中）
            set.remove_waiter(&waker);
            match Self::try_semop(set, sops)? {
                SemopOutcome::Done => {
                    return Ok(0);
                }
                SemopOutcome::Blocked { semnum, wait_type } => {
                    if non_blocking {
                        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                    }
                    if deadline_ticks.is_some_and(|deadline| clock() >= deadline) {
                        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                    }
                    if wait_interrupted {
                        return Err(SystemError::EINTR);
                    }
                    set.enqueue_waiter(SemWaiter {
                        semnum,
                        wait_type,
                        waker: waker.clone(),
                    });
                }
            }
            drop(guard);

            if let Some(t) = timer.as_ref() {
                t.activate();
            }
            let wait_result = waiter.wait(true);
            let was_timeout = timer.as_ref().is_some_and(|t| t.timeout());
            if !was_timeout {
                if let Some(t) = timer.as_ref() {
                    t.cancel();
                }
            }
            if wait_result.is_err() {
                wait_interrupted = true;
            }
        }
    }

    /// # IPC_RMID：删除信号量集合并唤醒所有等待者（返回 EIDRM）
    pub fn ipc_rmid(&mut self, id: SemId) -> Result<(), SystemError> {
        let decoded = IpcIdAllocator::decode(id.data())?;
        let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
        let key = {
            let set = self.get_by_semid_checked(id)?;
            ipc_perm::ipc_permission(&set.kern_ipc_perm, Self::IPC_WRITE, &target_user_ns)?;
            ipc_perm::check_control_permission(&set.kern_ipc_perm, &target_user_ns)?;
            set.kern_ipc_perm.key
        };
        let mut set = self.id2sem.remove(&decoded.idx).ok_or(SystemError::EINVAL)?;
        self.key2id.remove(&SemKey::new(key));
        self.id_allocator.free_idx(decoded.idx);
        self.total_sems = self.total_sems.saturating_sub(set.sems.len());
        set.wake_all();
        Ok(())
    }

    /// # IPC_SET：更新权限（uid/gid/mode）并刷新 sem_ctime
    pub fn ipc_set(&mut self, id: SemId, semid_ds: PosixSemIdDs) -> Result<(), SystemError> {
        let set = self.get_by_semid_checked_mut(id)?;
        let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
        ipc_perm::ipc_permission(&set.kern_ipc_perm, Self::IPC_WRITE, &target_user_ns)?;
        ipc_perm::check_control_permission(&set.kern_ipc_perm, &target_user_ns)?;
        let current_user_ns = ProcessManager::current_user_ns();
        set.kern_ipc_perm.copy_from_posix(
            semid_ds.sem_perm.uid(),
            semid_ds.sem_perm.gid(),
            semid_ds.sem_perm.mode(),
            &current_user_ns,
        )?;
        set.sem_ctime = PosixTimeSpec::now().tv_sec;
        Ok(())
    }

    /// IPC_STAT/SEM_STAT/SEM_STAT_ANY：输出 semid_ds
    pub fn sem_stat_data(
        &self,
        id: SemId,
        semnum: usize,
        cmd: SemCtlCmd,
    ) -> Result<(usize, PosixSemIdDs), SystemError> {
        let set = match cmd {
            SemCtlCmd::IpcStat => self.get_by_semid_checked(id)?,
            SemCtlCmd::SemStat | SemCtlCmd::SemStatAny => self.get_by_index(semnum)?,
            _ => return Err(SystemError::EINVAL),
        };
        if cmd != SemCtlCmd::SemStatAny {
            let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
            ipc_perm::ipc_permission(&set.kern_ipc_perm, Self::IPC_READ, &target_user_ns)?;
        }
        let current_user_ns = ProcessManager::current_user_ns();
        let sem_perm = set.kern_ipc_perm.to_posix(&current_user_ns)?;
        let semid_ds = PosixSemIdDs {
            sem_perm,
            sem_otime: set.sem_otime,
            _sem_otime_high: 0,
            sem_ctime: set.sem_ctime,
            _sem_ctime_high: 0,
            sem_nsems: set.sems.len(),
            _unused1: 0,
            _unused2: 0,
        };
        let ret = if cmd == SemCtlCmd::IpcStat {
            0
        } else {
            set.kern_ipc_perm.id
        };
        Ok((ret, semid_ds))
    }

    /// IPC_INFO/SEM_INFO：输出系统信息
    pub fn sem_info_data(&self) -> (usize, PosixSemInfo) {
        (self.current_max_index(), PosixSemInfo::new())
    }

    /// GETVAL/GETPID/GETNCNT/GETZCNT：单信号量查询
    pub fn sem_get_value(
        &self,
        id: SemId,
        semnum: usize,
        cmd: SemCtlCmd,
    ) -> Result<usize, SystemError> {
        let set = self.get_by_semid_checked(id)?;
        if semnum >= set.sems.len() {
            return Err(SystemError::EINVAL);
        }
        let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
        ipc_perm::ipc_permission(&set.kern_ipc_perm, Self::IPC_READ, &target_user_ns)?;
        match cmd {
            SemCtlCmd::GetVal => Ok(set.sems[semnum].val as usize),
            SemCtlCmd::GetPid => Ok(set.sems[semnum].pid.data()),
            SemCtlCmd::GetNcnt => Ok(set.ncnt(semnum)),
            SemCtlCmd::GetZcnt => Ok(set.zcnt(semnum)),
            _ => Err(SystemError::EINVAL),
        }
    }

    /// # SETVAL：设置单个信号量的值
    pub fn setval(&mut self, id: SemId, semnum: usize, val: i32) -> Result<(), SystemError> {
        // 与 Linux 一致：先校验值（ERANGE），再 semnum（EINVAL），再权限（EACCES）
        if !(0..=SEMVMX).contains(&val) {
            return Err(SystemError::ERANGE);
        }
        let set = self.get_by_semid_checked_mut(id)?;
        if semnum >= set.sems.len() {
            return Err(SystemError::EINVAL);
        }
        let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
        ipc_perm::ipc_permission(&set.kern_ipc_perm, Self::IPC_WRITE, &target_user_ns)?;

        let sem = &mut set.sems[semnum];
        let old = sem.val;
        sem.val = val;
        sem.pid = ProcessManager::current_pid();
        let mut changes = Vec::new();
        if sem.val > old {
            changes.push(SemChange::Increase(semnum));
        }
        if sem.val == 0 {
            changes.push(SemChange::Zero(semnum));
        }
        set.sem_ctime = PosixTimeSpec::now().tv_sec;
        set.wake_waiters(&changes);
        Ok(())
    }

    /// # SETALL：设置集合内所有信号量的值（校验失败时不改变任何值）
    pub fn setall(&mut self, id: SemId, vals: &[u16]) -> Result<(), SystemError> {
        let set = self.get_by_semid_checked_mut(id)?;
        if vals.len() != set.sems.len() {
            return Err(SystemError::EINVAL);
        }
        let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
        ipc_perm::ipc_permission(&set.kern_ipc_perm, Self::IPC_WRITE, &target_user_ns)?;
        if vals.iter().any(|&v| v as i32 > SEMVMX) {
            return Err(SystemError::ERANGE);
        }

        let pid = ProcessManager::current_pid();
        let mut changes = Vec::new();
        for (i, &v) in vals.iter().enumerate() {
            let sem = &mut set.sems[i];
            let old = sem.val;
            sem.val = v as i32;
            sem.pid = pid;
            if sem.val > old {
                changes.push(SemChange::Increase(i));
            }
            if sem.val == 0 {
                changes.push(SemChange::Zero(i));
            }
        }
        set.sem_ctime = PosixTimeSpec::now().tv_sec;
        set.wake_waiters(&changes);
        Ok(())
    }

    /// # GETALL：获取集合内所有信号量的值
    pub fn getall(&self, id: SemId) -> Result<Vec<u16>, SystemError> {
        let set = self.get_by_semid_checked(id)?;
        let target_user_ns = ProcessManager::current_ipcns().user_ns.clone();
        ipc_perm::ipc_permission(&set.kern_ipc_perm, Self::IPC_READ, &target_user_ns)?;
        Ok(set.sems.iter().map(|s| s.val as u16).collect())
    }

    /// 集合内信号量数量（供 SETALL 在拷贝用户数组前确定长度）
    pub fn nsems(&self, id: SemId) -> Result<usize, SystemError> {
        let set = self.get_by_semid_checked(id)?;
        Ok(set.sems.len())
    }
}
