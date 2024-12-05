use core::sync::atomic::AtomicUsize;

use alloc::vec::Vec;

const GLOBAL_ROOT_UID: Kuid = Kuid(0);
const GLOBAL_ROOT_GID: Kgid = Kgid(0);
pub static INIT_CRED: Cred = Cred::init();

int_like!(Kuid, AtomicKuid, usize, AtomicUsize);
int_like!(Kgid, AtomicKgid, usize, AtomicUsize);

bitflags! {
    pub struct CAPFlags:u64{
        const CAP_EMPTY_SET = 0;
        const CAP_FULL_SET = (1 << 41) - 1;
    }
}

pub enum CredFsCmp {
    Equal,
    Less,
    Greater,
}

/// 凭证集
#[derive(Debug, Clone, PartialEq, Eq)]
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
}

impl Cred {
    pub const fn init() -> Self {
        Self {
            uid: GLOBAL_ROOT_UID,
            gid: GLOBAL_ROOT_GID,
            suid: GLOBAL_ROOT_UID,
            sgid: GLOBAL_ROOT_GID,
            euid: GLOBAL_ROOT_UID,
            egid: GLOBAL_ROOT_GID,
            fsuid: GLOBAL_ROOT_UID,
            fsgid: GLOBAL_ROOT_GID,
            cap_inheritable: CAPFlags::CAP_EMPTY_SET,
            cap_permitted: CAPFlags::CAP_FULL_SET,
            cap_effective: CAPFlags::CAP_FULL_SET,
            cap_bset: CAPFlags::CAP_FULL_SET,
            cap_ambient: CAPFlags::CAP_FULL_SET,
            group_info: None,
        }
    }

    #[allow(dead_code)]
    /// Compare two credentials with respect to filesystem access.
    pub fn fscmp(&self, other: Cred) -> CredFsCmp {
        if *self == other {
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
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GroupInfo {
    pub gids: Vec<Kgid>,
}
