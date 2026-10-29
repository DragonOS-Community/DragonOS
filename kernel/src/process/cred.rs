use alloc::sync::Arc;
use core::sync::atomic::{AtomicI32, AtomicUsize};

use alloc::vec::Vec;

use super::namespace::user_namespace::{UserNamespace, INIT_USER_NAMESPACE};
use crate::process::ProcessManager;
use crate::security::keys::{KeyRef, SpecialKeyringKind};
use system_error::SystemError;

const GLOBAL_ROOT_UID: Kuid = Kuid(0);
const GLOBAL_ROOT_GID: Kgid = Kgid(0);
lazy_static::lazy_static! {
    pub static ref INIT_CRED: Arc<Cred> = Cred::init();
}

int_like!(Kuid, AtomicKuid, usize, AtomicUsize);
int_like!(Kgid, AtomicKgid, usize, AtomicUsize);

/// suid_dumpable value: core dumps fully disabled
pub const SUID_DUMP_DISABLE: i32 = 0;
/// suid_dumpable value: dump by the normal user-process rules
pub const SUID_DUMP_USER: i32 = 1;
/// suid_dumpable value: root suid binaries may also dump.
#[allow(dead_code)]
pub const SUID_DUMP_ROOT: i32 = 2;

/// Global suid_dumpable switch (/proc/sys/fs/suid_dumpable).
pub static SUID_DUMPABLE: AtomicI32 = AtomicI32::new(SUID_DUMP_DISABLE);

/// Highest capability number recognized by this kernel (Linux 6.6: CAP_CHECKPOINT_RESTORE).
pub const CAP_LAST_CAP: usize = 40;

/// Linux's initial destination for request_key() results.
pub const KEY_REQKEY_DEFL_THREAD_KEYRING: i32 = 1;

bitflags! {
    pub struct CAPFlags:u64{
        const CAP_EMPTY_SET = 0;
        const CAP_FULL_SET = (1u64 << (CAP_LAST_CAP + 1)) - 1;

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
        const CAP_CHECKPOINT_RESTORE = 1 << CAP_LAST_CAP;
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
    /// Linux SECURE_KEEP_CAPS (a credential property, cleared by userns and exec).
    pub keepcaps: bool,
    pub user_ns: Arc<UserNamespace>,
    /// Credentials own the task's keyring references.  Links in a keyring
    /// remain shared across credential copies, as in Linux.
    pub thread_keyring: Option<KeyRef>,
    pub process_keyring: Option<KeyRef>,
    pub session_keyring: Option<KeyRef>,
    pub request_key_auth: Option<KeyRef>,
    pub jit_keyring: i32,
}

impl Cred {
    fn init() -> Arc<Self> {
        // 默认 init 进程能力集对齐 Linux init_cred：
        // permitted/effective/bset 为 full set，ambient 为空。
        let init_caps = CAPFlags::CAP_FULL_SET;
        Arc::new(Self {
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
            keepcaps: false,
            user_ns: INIT_USER_NAMESPACE.clone(),
            thread_keyring: None,
            process_keyring: None,
            session_keyring: None,
            request_key_auth: None,
            jit_keyring: KEY_REQKEY_DEFL_THREAD_KEYRING,
        })
    }

    pub fn new_arc(cred: Cred) -> Arc<Self> {
        Arc::new(cred)
    }

    /// Use when preparing a credential in a fallible fork or exec phase.
    pub fn try_new_arc(cred: Cred) -> Result<Arc<Self>, SystemError> {
        Arc::try_new(cred).map_err(|_| SystemError::ENOMEM)
    }

    /// Linux copy_creds(): a new thread receives a fresh thread keyring if
    /// the creator had one; a new process never inherits the process keyring.
    /// A session keyring and request-key context are copied by reference.
    pub fn prepare_fork_keyrings(
        &self,
        clone_thread: bool,
    ) -> Result<Option<Arc<Self>>, SystemError> {
        let needs_copy =
            self.thread_keyring.is_some() || (!clone_thread && self.process_keyring.is_some());
        if !needs_copy {
            return Ok(None);
        }

        let mut next = self.clone();
        if self.thread_keyring.is_some() {
            next.thread_keyring = if clone_thread {
                Some(crate::security::keys::create_special_keyring(
                    &next,
                    SpecialKeyringKind::Thread,
                )?)
            } else {
                None
            };
        }
        if !clone_thread {
            next.process_keyring = None;
        }
        Ok(Some(Self::try_new_arc(next)?))
    }

    /// Prepare before exec's last fallible operation.  The successful commit
    /// then only publishes an already allocated credential.
    pub fn prepare_exec_keyrings(&self) -> Result<Option<Arc<Self>>, SystemError> {
        if !self.keepcaps && self.thread_keyring.is_none() && self.process_keyring.is_none() {
            return Ok(None);
        }
        let mut next = self.clone();
        next.keepcaps = false;
        next.thread_keyring = None;
        next.process_keyring = None;
        Ok(Some(Self::try_new_arc(next)?))
    }

    /// Linux commit_creds() updates the permission owner of an existing
    /// thread ring when fsuid/fsgid changes, without moving its quota charge.
    /// Call before acquiring the task publication lock: the key state mutex
    /// may sleep and must never nest below task_lock.
    pub(crate) fn sync_thread_keyring_owner(&self, old: &Cred) {
        if self.fsuid != old.fsuid || self.fsgid != old.fsgid {
            if let Some(thread_keyring) = self.thread_keyring.as_ref() {
                crate::security::keys::set_key_owner_uid_gid(
                    thread_keyring,
                    self.fsuid,
                    self.fsgid,
                );
            }
        }
    }

    #[allow(dead_code)]
    /// Compare two credentials with respect to filesystem access.
    pub fn fscmp(&self, other: Arc<Cred>) -> CredFsCmp {
        if core::ptr::eq(self, other.as_ref()) {
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

        if self.groups == other.groups {
            return CredFsCmp::Equal;
        }
        match self.groups.cmp(&other.groups) {
            core::cmp::Ordering::Less => CredFsCmp::Less,
            core::cmp::Ordering::Equal => CredFsCmp::Equal,
            core::cmp::Ordering::Greater => CredFsCmp::Greater,
        }
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

    /// Check whether the current process has the given capability in the specified (usually the target task's) user namespace.
    pub fn has_capability_in_ns(&self, targ_ns: &Arc<UserNamespace>, cap: CAPFlags) -> bool {
        cap_capable(self, targ_ns, cap)
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
        if let Some(parent_ns) = ns.parent.as_ref() {
            if Arc::ptr_eq(parent_ns, &cred.user_ns) && ns_owner == cred.euid.data() {
                return true;
            }
            ns = parent_ns.clone();
            continue;
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

/// Whether the current task may use perf's privileged monitoring features.
///
/// This matches Linux 6.6 `perfmon_capable()`: `CAP_PERFMON` is the least
/// privileged authorization, while `CAP_SYS_ADMIN` remains a compatibility
/// fallback for existing administrative callers.
pub fn perfmon_capable() -> bool {
    capable(CAPFlags::CAP_PERFMON) || capable(CAPFlags::CAP_SYS_ADMIN)
}

/// Whether new's permitted caps are a subset of old's (no privilege raise).
pub fn cred_cap_issubset(old: &Cred, new: &Cred) -> bool {
    let mut ns = new.user_ns.clone();
    // Same namespace: compare the cap sets directly
    if Arc::ptr_eq(&old.user_ns, &ns) {
        return (new.cap_permitted.bits() & !old.cap_permitted.bits()) == 0;
    }
    // Across namespaces: walk the parent chain checking each owner against old's euid
    while !Arc::ptr_eq(&ns, &INIT_USER_NAMESPACE) {
        let ns_owner = ns.inner.lock().owner;
        let Some(parent_ns) = ns.parent.as_ref() else {
            return false;
        };
        if Arc::ptr_eq(&old.user_ns, parent_ns) && ns_owner == old.euid.data() {
            return true;
        }
        ns = parent_ns.clone();
    }
    false
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
    cred.keepcaps = false;
    cred.cap_inheritable = CAPFlags::CAP_EMPTY_SET;
    cred.cap_permitted = CAPFlags::CAP_FULL_SET;
    cred.cap_effective = CAPFlags::CAP_FULL_SET;
    cred.cap_ambient = CAPFlags::CAP_EMPTY_SET;
    cred.cap_bset = CAPFlags::CAP_FULL_SET;
    cred.user_ns = user_ns;
}
