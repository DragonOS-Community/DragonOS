use alloc::sync::{Arc, Weak};
use core::sync::atomic::AtomicUsize;

use alloc::vec::Vec;

use super::namespace::user_namespace::{UserNamespace, INIT_USER_NAMESPACE};
use crate::process::ProcessManager;

const GLOBAL_ROOT_UID: Kuid = Kuid(0);
const GLOBAL_ROOT_GID: Kgid = Kgid(0);
lazy_static::lazy_static! {
    pub static ref INIT_CRED: Arc<Cred> = Cred::init();
}

int_like!(Kuid, AtomicKuid, usize, AtomicUsize);
int_like!(Kgid, AtomicKgid, usize, AtomicUsize);

bitflags! {
    pub struct CAPFlags:u64{
        const CAP_EMPTY_SET = 0;
        const CAP_FULL_SET = (1 << 41) - 1;

        // 具体的capability定义，与Linux保持一致
        const CAP_CHOWN = 1 << 0;
        const CAP_DAC_OVERRIDE = 1 << 1;
        const CAP_DAC_READ_SEARCH = 1 << 2;
        const CAP_FOWNER = 1 << 3;
        const CAP_FSETID = 1 << 4;
        const CAP_KILL = 1 << 5;
        const CAP_SETGID = 1 << 6;
        const CAP_SETUID = 1 << 7;
        const CAP_SETPCAP = 1 << 8;
        const CAP_LINUX_IMMUTABLE = 1 << 9;
        const CAP_NET_BIND_SERVICE = 1 << 10;
        const CAP_NET_BROADCAST = 1 << 11;
        const CAP_NET_ADMIN = 1 << 12;
        const CAP_NET_RAW = 1 << 13;
        const CAP_IPC_LOCK = 1 << 14;
        const CAP_IPC_OWNER = 1 << 15;
        const CAP_SYS_MODULE = 1 << 16;
        const CAP_SYS_RAWIO = 1 << 17;
        const CAP_SYS_CHROOT = 1 << 18;
        const CAP_SYS_PTRACE = 1 << 19;
        const CAP_SYS_PACCT = 1 << 20;
        const CAP_SYS_ADMIN = 1 << 21;
        const CAP_SYS_BOOT = 1 << 22;
        const CAP_SYS_NICE = 1 << 23;
        const CAP_SYS_RESOURCE = 1 << 24;
        const CAP_SYS_TIME = 1 << 25;
        const CAP_SYS_TTY_CONFIG = 1 << 26;
        const CAP_MKNOD = 1 << 27;
        const CAP_LEASE = 1 << 28;
        const CAP_AUDIT_WRITE = 1 << 29;
        const CAP_AUDIT_CONTROL = 1 << 30;
        const CAP_SETFCAP = 1 << 31;
        const CAP_MAC_OVERRIDE = 1 << 32;
        const CAP_MAC_ADMIN = 1 << 33;
        const CAP_SYSLOG = 1 << 34;
        const CAP_WAKE_ALARM = 1 << 35;
        const CAP_BLOCK_SUSPEND = 1 << 36;
        const CAP_AUDIT_READ = 1 << 37;
        const CAP_PERFMON = 1 << 38;
        const CAP_BPF = 1 << 39;
        const CAP_CHECKPOINT_RESTORE = 1 << 40;
    }
}

pub enum CredFsCmp {
    Equal,
    Less,
    Greater,
}

/// 凭证集
#[derive(Debug, Clone)]
pub struct Cred {
    pub self_ref: Weak<Cred>,
    /// 进程实际uid
    pub uid: Kuid,
    /// 进程实际gid
    pub gid: Kgid,
    /// 进程保存的uid
    pub suid: Kuid,
    /// 进程保存的gid
    pub sgid: Kgid,
    /// 进程有效的uid
    pub euid: Kuid,
    /// 进程有效的gid
    pub egid: Kgid,
    /// supplementary groups
    pub groups: Vec<Kgid>,
    /// UID for VFS ops
    pub fsuid: Kuid,
    /// GID for VFS ops
    pub fsgid: Kgid,
    /// 子进程可以继承的权限
    pub cap_inheritable: CAPFlags,
    /// 当前进程被赋予的权限
    pub cap_permitted: CAPFlags,
    /// 当前进程实际使用的权限
    pub cap_effective: CAPFlags,
    /// capability bounding set
    pub cap_bset: CAPFlags,
    /// Ambient capability set
    pub cap_ambient: CAPFlags,
    /// supplementary groups for euid/fsgid
    pub group_info: Option<GroupInfo>,
    pub user_ns: Arc<UserNamespace>,
}

impl Cred {
    fn init() -> Arc<Self> {
        // 默认 init 进程能力集对齐 Linux init_cred：
        // permitted/effective/bset 为 full set，ambient 为空。
        let init_caps = CAPFlags::CAP_FULL_SET;
        Arc::new_cyclic(|weak_self| Self {
            self_ref: weak_self.clone(),
            uid: GLOBAL_ROOT_UID,
            gid: GLOBAL_ROOT_GID,
            suid: GLOBAL_ROOT_UID,
            sgid: GLOBAL_ROOT_GID,
            euid: GLOBAL_ROOT_UID,
            egid: GLOBAL_ROOT_GID,
            fsuid: GLOBAL_ROOT_UID,
            fsgid: GLOBAL_ROOT_GID,
            groups: Vec::new(),
            cap_inheritable: CAPFlags::CAP_EMPTY_SET,
            cap_permitted: init_caps,
            cap_effective: init_caps,
            cap_bset: init_caps,
            cap_ambient: CAPFlags::CAP_EMPTY_SET,
            group_info: None,
            user_ns: INIT_USER_NAMESPACE.clone(),
        })
    }

    pub fn new_arc(cred: Cred) -> Arc<Self> {
        Arc::new_cyclic(|weak_self| {
            let mut new_cred = cred;
            new_cred.self_ref = weak_self.clone();
            new_cred
        })
    }

    #[allow(dead_code)]
    /// Compare two credentials with respect to filesystem access.
    pub fn fscmp(&self, other: Arc<Cred>) -> CredFsCmp {
        if Arc::ptr_eq(&self.self_ref.upgrade().unwrap(), &other) {
            return CredFsCmp::Equal;
        }

        if self.fsuid < other.fsuid {
            return CredFsCmp::Less;
        }
        if self.fsuid > other.fsuid {
            return CredFsCmp::Greater;
        }

        if self.fsgid < other.fsgid {
            return CredFsCmp::Less;
        }
        if self.fsgid > other.fsgid {
            return CredFsCmp::Greater;
        }

        if self.group_info == other.group_info {
            return CredFsCmp::Equal;
        }

        if let (Some(ga), Some(gb)) = (&self.group_info, &other.group_info) {
            let ga_count = ga.gids.len();
            let gb_count = gb.gids.len();

            if ga_count < gb_count {
                return CredFsCmp::Less;
            }
            if ga_count > gb_count {
                return CredFsCmp::Greater;
            }

            for i in 0..ga_count {
                if ga.gids[i] < gb.gids[i] {
                    return CredFsCmp::Less;
                }
                if ga.gids[i] > gb.gids[i] {
                    return CredFsCmp::Greater;
                }
            }
        } else {
            if self.group_info.is_none() {
                return CredFsCmp::Less;
            }
            if other.group_info.is_none() {
                return CredFsCmp::Greater;
            }
        }

        return CredFsCmp::Equal;
    }

    pub fn setuid(&mut self, uid: usize) {
        self.uid.0 = uid;
    }

    pub fn seteuid(&mut self, euid: usize) {
        self.euid.0 = euid;
    }

    pub fn setsuid(&mut self, suid: usize) {
        self.suid.0 = suid;
    }

    pub fn setfsuid(&mut self, fsuid: usize) {
        self.fsuid.0 = fsuid;
    }

    pub fn setgid(&mut self, gid: usize) {
        self.gid.0 = gid;
    }

    pub fn setegid(&mut self, egid: usize) {
        self.egid.0 = egid;
    }

    pub fn setsgid(&mut self, sgid: usize) {
        self.sgid.0 = sgid;
    }

    pub fn setfsgid(&mut self, fsgid: usize) {
        self.fsgid.0 = fsgid;
    }

    /// Set supplementary groups
    pub fn setgroups(&mut self, groups: Vec<Kgid>) {
        self.groups = groups;
    }

    /// Get supplementary groups
    pub fn getgroups(&self) -> &Vec<Kgid> {
        &self.groups
    }

    /// 检查当前进程是否具有指定的capability（在当前 user_ns 中）
    pub fn has_capability(&self, cap: CAPFlags) -> bool {
        cap_capable(self, &self.user_ns, cap)
    }

    /// 检查当前进程是否具有CAP_SYS_ADMIN权限
    pub fn has_cap_sys_admin(&self) -> bool {
        self.has_capability(CAPFlags::CAP_SYS_ADMIN)
    }
}

/// 检查 cred 在目标 user namespace 中是否具有指定 capability
///
/// 遵循 Linux cap_capable 的层次遍历规则：
/// 1. 如果 cred 就在目标 ns 中，检查 effective cap
/// 2. 如果 cred 的 ns 比目标更浅，返回 false
/// 3. 如果 cred 的用户是目标 ns 的直接 owner，返回 true
/// 4. 否则向上遍历 parent chain
pub fn cap_capable(cred: &Cred, targ_ns: &Arc<UserNamespace>, cap: CAPFlags) -> bool {
    let mut ns = targ_ns.clone();
    loop {
        if Arc::ptr_eq(&cred.user_ns, &ns) {
            return cred.cap_effective.contains(cap);
        }

        if cred.user_ns.level() >= ns.level() {
            return false;
        }

        let ns_owner = ns.inner.lock().owner;
        if let Some(ref parent_weak) = ns.parent {
            if let Some(parent_ns) = parent_weak.upgrade() {
                if Arc::ptr_eq(&parent_ns, &cred.user_ns) && ns_owner == cred.euid.data() {
                    return true;
                }
                ns = parent_ns;
                continue;
            }
        }

        return false;
    }
}

/// 检查当前进程在指定 ns 中是否有某 capability
pub fn ns_capable(ns: &Arc<UserNamespace>, cap: CAPFlags) -> bool {
    let pcb = ProcessManager::current_pcb();
    cap_capable(&pcb.cred(), ns, cap)
}

/// Check whether the current process has a capability in the initial user namespace.
///
/// Linux `capable(cap)` is equivalent to `ns_capable(&init_user_ns, cap)`.
/// Hardware-level capabilities such as `CAP_SYS_RAWIO` must use this semantic
/// instead of checking the current process user namespace.
pub fn capable(cap: CAPFlags) -> bool {
    ns_capable(&INIT_USER_NAMESPACE, cap)
}

/// 检查当前进程在指定 ns 中是否有某 capability（setid 上下文）
pub fn ns_capable_setid(ns: &Arc<UserNamespace>, cap: CAPFlags) -> bool {
    ns_capable(ns, cap)
}

/// 当进程进入新的 user namespace 时重置 credentials
///
/// 遵循 Linux 语义：
/// - 能力集重置为 FULL（在新 ns 中有全部能力）
/// - securebits 重置为默认值
/// - uid/gid/euid/egid/fsuid/fsgid **不改变**
/// - user_ns 指向新的 namespace
pub fn set_cred_user_ns(cred: &mut Cred, user_ns: Arc<UserNamespace>) {
    cred.cap_inheritable = CAPFlags::CAP_EMPTY_SET;
    cred.cap_permitted = CAPFlags::CAP_FULL_SET;
    cred.cap_effective = CAPFlags::CAP_FULL_SET;
    cred.cap_ambient = CAPFlags::CAP_EMPTY_SET;
    cred.cap_bset = CAPFlags::CAP_FULL_SET;
    cred.user_ns = user_ns;
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GroupInfo {
    pub gids: Vec<Kgid>,
}
