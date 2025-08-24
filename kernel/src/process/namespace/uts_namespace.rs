use core::ops::Deref;

use alloc::string::{String, ToString};

use alloc::sync::{Arc, Weak};

use cfg_if::cfg_if;
use system_error::SystemError;

use crate::libs::spinlock::SpinLockGuard;
use crate::process::fork::CloneFlags;
use crate::process::namespace::user_namespace::INIT_USER_NAMESPACE;
use crate::process::namespace::NamespaceType;
use crate::process::ProcessManager;
use crate::{
    libs::spinlock::SpinLock,
    process::namespace::{nsproxy::NsCommon, user_namespace::UserNamespace},
};

lazy_static! {
    pub static ref INIT_UTS_NAMESPACE: Arc<UtsNamespace> = UtsNamespace::new_root();
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PosixNewUtsName {
    pub sysname: [u8; NewUtsName::MAXLEN],
    pub nodename: [u8; NewUtsName::MAXLEN],
    pub release: [u8; NewUtsName::MAXLEN],
    pub version: [u8; NewUtsName::MAXLEN],
    pub machine: [u8; NewUtsName::MAXLEN],
    pub domainname: [u8; NewUtsName::MAXLEN],
}

impl From<&NewUtsName> for PosixNewUtsName {
    #[inline(never)]
    fn from(new_name: &NewUtsName) -> Self {
        let mut result = PosixNewUtsName::zeroed();

        copy_string_to_array(&new_name.sys_name, &mut result.sysname);
        copy_string_to_array(&new_name.node_name, &mut result.nodename);
        copy_string_to_array(&new_name.release, &mut result.release);
        copy_string_to_array(&new_name.version, &mut result.version);
        copy_string_to_array(&new_name.machine, &mut result.machine);
        copy_string_to_array(&new_name.domain_name, &mut result.domainname);

        result
    }
}

impl PosixNewUtsName {
    fn zeroed() -> Self {
        Self {
            sysname: [0; NewUtsName::MAXLEN],
            nodename: [0; NewUtsName::MAXLEN],
            release: [0; NewUtsName::MAXLEN],
            version: [0; NewUtsName::MAXLEN],
            machine: [0; NewUtsName::MAXLEN],
            domainname: [0; NewUtsName::MAXLEN],
        }
    }
}

#[inline(never)]
fn copy_string_to_array(src: &str, dst: &mut [u8; 65]) {
    let bytes = src.as_bytes();
    let len = bytes.len().min(64);
    dst[0..len].copy_from_slice(&bytes[0..len]);
    dst[len] = 0;
}

#[derive(Clone)]
pub struct NewUtsName {
    sys_name: String,
    node_name: String,
    release: String,
    version: String,
    machine: String,
    domain_name: String,
}

impl NewUtsName {
    pub const MAXLEN: usize = 65;
    fn validate_str(s: &str) -> bool {
        s.len() < Self::MAXLEN
    }
}

impl Default for NewUtsName {
    fn default() -> Self {
        Self {
            sys_name: UtsNamespace::UTS_SYSNAME.to_string(),
            node_name: UtsNamespace::UTS_NODENAME.to_string(),
            release: UtsNamespace::UTS_RELEASE.to_string(),
            version: UtsNamespace::UTS_VERSION.to_string(),
            machine: UtsNamespace::UTS_MACHINE.to_string(),
            domain_name: UtsNamespace::UTS_DOMAINNAME.to_string(),
        }
    }
}

pub struct UtsNamespace {
    ns_common: NsCommon,
    self_ref: Weak<UtsNamespace>,
    /// 关联的 user namespace (权限判断使用)
    _user_ns: Arc<UserNamespace>,
    utsname: SpinLock<NewUtsName>,
}

impl UtsNamespace {
    const UTS_SYSNAME: &str = "Linux";
    const UTS_NODENAME: &str = "dragonos";
    const UTS_RELEASE: &str = "6.6.21";
    const UTS_VERSION: &str = "6.6.21";

    cfg_if! {
        if #[cfg(target_arch = "x86_64")] {
            const UTS_MACHINE: &str = "x86_64";
        } else if #[cfg(target_arch = "aarch64")] {
            const UTS_MACHINE: &str = "aarch64";
        } else if #[cfg(target_arch = "riscv64")] {
            const UTS_MACHINE: &str = "riscv64";
        } else if #[cfg(target_arch = "loongarch64")] {
            const UTS_MACHINE: &str = "loongarch64";
        } else {
            const UTS_MACHINE: &str = "unknown";
        }
    }

    const UTS_DOMAINNAME: &str = "(none)";

    fn new_root() -> Arc<Self> {
        Arc::new_cyclic(|weak_self| Self {
            ns_common: NsCommon::new(0, NamespaceType::Uts),
            self_ref: weak_self.clone(),
            _user_ns: INIT_USER_NAMESPACE.clone(),
            utsname: SpinLock::new(NewUtsName::default()),
        })
    }

    #[inline(never)]
    pub fn copy_uts_ns(
        &self,
        clone_flags: &CloneFlags,
        user_ns: Arc<UserNamespace>,
    ) -> Result<Arc<UtsNamespace>, SystemError> {
        if !clone_flags.contains(CloneFlags::CLONE_NEWUTS) {
            // 不创建新的uts namespace，直接引用当前的
            return Ok(self.self_ref.upgrade().unwrap());
        }

        let new_uts_ns = Arc::new_cyclic(|weak_self| UtsNamespace {
            ns_common: NsCommon::new(self.ns_common.level + 1, NamespaceType::Uts),
            self_ref: weak_self.clone(),
            _user_ns: user_ns,
            utsname: SpinLock::new(self.utsname.lock().clone()),
        });

        Ok(new_uts_ns)
    }

    pub fn utsname(&self) -> ReadOnlyUtsNameWrapper<'_> {
        ReadOnlyUtsNameWrapper {
            inner: self.utsname.lock(),
        }
    }

    pub fn set_hostname(&self, hostname: &str) -> Result<(), SystemError> {
        // 验证长度
        if !NewUtsName::validate_str(hostname) {
            return Err(SystemError::ENAMETOOLONG);
        }

        // 检查权限（需要 CAP_SYS_ADMIN）
        // TODO: 实现完整的 capability 检查
        if !self.check_uts_modify_permission() {
            return Err(SystemError::EPERM);
        }
        let s = hostname.to_string();
        let mut utsname = self.utsname.lock();
        utsname.node_name = s;
        Ok(())
    }

    /// 检查是否有权限修改 UTS 信息
    fn check_uts_modify_permission(&self) -> bool {
        // TODO: 实现完整的 capability 检查
        // 目前暂时返回 true，后续需要检查 CAP_SYS_ADMIN
        true
    }
}

pub struct ReadOnlyUtsNameWrapper<'a> {
    inner: SpinLockGuard<'a, NewUtsName>,
}

impl<'a> Deref for ReadOnlyUtsNameWrapper<'a> {
    type Target = NewUtsName;
    fn deref(&self) -> &Self::Target {
        self.inner.deref()
    }
}

impl ProcessManager {
    pub fn current_utsns() -> Arc<UtsNamespace> {
        if Self::initialized() {
            ProcessManager::current_pcb().nsproxy.read().uts_ns.clone()
        } else {
            INIT_UTS_NAMESPACE.clone()
        }
    }
}
