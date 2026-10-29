// SPDX-License-Identifier: GPL-2.0-or-later
//! Linux semaphore IDs, flags and userspace layouts.
use crate::ipc::ipc_perm::PosixIpcPerm;
use core::fmt;

pub const IPC_PRIVATE: SemKey = SemKey::new(0);

int_like!(SemId, usize);
int_like!(SemKey, usize);

pub const SEMMNI: usize = 32000;
pub const SEMMSL: usize = 32000;
pub const SEMMNS: usize = SEMMNI * SEMMSL;
pub const SEMOPM: usize = 500;
pub const SEMVMX: i32 = 32767;

bitflags! {
    pub struct SemFlags: u32 {
        const PERM_MASK = 0o777;
        const IPC_CREAT = 0o1000;
        const IPC_EXCL = 0o2000;
        const IPC_NOWAIT = 0x800;
        const SEM_UNDO = 0x1000;
    }
}

/// Semaphore-set control commands (Linux x86_64 UAPI include/uapi/linux/sem.h)
#[derive(Eq, Clone, Copy)]
pub enum SemCtlCmd {
    /// Remove the semaphore set
    IpcRmid = 0,
    /// Set permissions
    IpcSet = 1,
    /// Retrieve `SemIdDs`
    IpcStat = 2,
    /// Retrieve `SemInfo`
    IpcInfo = 3,
    /// Get the PID of the last process to operate on the specified semaphore
    GetPid = 11,
    /// Get the specified semaphore value
    GetVal = 12,
    /// Get values of all semaphores in the set
    GetAll = 13,
    /// Get the number of processes waiting for the specified semaphore to increase
    GetNcnt = 14,
    /// Get the number of processes waiting for the specified semaphore to reach zero
    GetZcnt = 15,
    /// Set the specified semaphore value
    SetVal = 16,
    /// Set values of all semaphores in the set
    SetAll = 17,
    /// Retrieve `SemIdDs` by index
    SemStat = 18,
    /// Retrieve `SemInfo`
    SemInfo = 19,
    /// Retrieve `SemIdDs` by index without permission checks
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

/// Userspace `sembuf` (Linux `struct sembuf`, 6 bytes)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PosixSemBuf {
    pub sem_num: u16,
    pub sem_op: i16,
    pub sem_flg: i16,
}

/// Linux `semid64_ds`: x86_64 has historical padding after each 64-bit time;
/// the supported asm-generic 64-bit architectures do not.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct PosixSemIdDs {
    /// Permission information
    pub sem_perm: PosixIpcPerm,
    /// Time of the last `semop`
    pub sem_otime: i64,
    #[cfg(target_arch = "x86_64")]
    _otime_padding: u64,
    /// Time of the last metadata change
    pub sem_ctime: i64,
    #[cfg(target_arch = "x86_64")]
    _ctime_padding: u64,
    /// Number of semaphores in the set
    pub sem_nsems: usize,
    _unused1: usize,
    _unused2: usize,
}

// Keep the copy size and field positions tied to the target Linux UAPI.
const _: () = {
    assert!(core::mem::offset_of!(PosixSemIdDs, sem_otime) == 48);
    if cfg!(target_arch = "x86_64") {
        assert!(core::mem::size_of::<PosixSemIdDs>() == 104);
        assert!(core::mem::offset_of!(PosixSemIdDs, sem_ctime) == 64);
        assert!(core::mem::offset_of!(PosixSemIdDs, sem_nsems) == 80);
    } else {
        assert!(core::mem::size_of::<PosixSemIdDs>() == 88);
        assert!(core::mem::offset_of!(PosixSemIdDs, sem_ctime) == 56);
        assert!(core::mem::offset_of!(PosixSemIdDs, sem_nsems) == 64);
    }
};

/// Semaphore system information matching Linux `struct seminfo` (40 bytes)
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
    pub(super) fn new(cmd: SemCtlCmd, set_count: usize, total_sems: usize) -> Self {
        let (semusz, semaem) = if cmd == SemCtlCmd::SemInfo {
            (set_count as i32, total_sems as i32)
        } else {
            (20, SEMVMX)
        };
        PosixSemInfo {
            semmap: SEMMNS as i32,
            semmni: SEMMNI as i32,
            semmns: SEMMNS as i32,
            semmnu: SEMMNS as i32,
            semmsl: SEMMSL as i32,
            semopm: SEMOPM as i32,
            semume: SEMOPM as i32,
            semusz,
            semvmx: SEMVMX,
            semaem,
        }
    }
}

impl PosixSemIdDs {
    pub(super) fn new(
        sem_perm: PosixIpcPerm,
        sem_otime: i64,
        sem_ctime: i64,
        sem_nsems: usize,
    ) -> Self {
        Self {
            sem_perm,
            sem_otime,
            sem_ctime,
            sem_nsems,
            ..Self::default()
        }
    }
}
