use core::ops::Deref;

use alloc::sync::{Arc, Weak};

use cfg_if::cfg_if;
use system_error::SystemError;

use crate::init::version_info::get_kernel_build_info;
use crate::libs::spinlock::SpinLockGuard;
use crate::process::fork::CloneFlags;
use crate::process::namespace::user_namespace::INIT_USER_NAMESPACE;
use crate::process::namespace::{NamespaceOps, NamespaceType};
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

        result.sysname = new_name.sys_name;
        result.nodename = new_name.node_name;
        result.release = new_name.release;
        result.version = new_name.version;
        result.machine = new_name.machine;
        result.domainname = new_name.domain_name;

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
fn copy_bytes_to_array(src: &[u8], dst: &mut [u8; 65]) {
    dst.fill(0);
    let len = src.len().min(64);
    dst[0..len].copy_from_slice(&src[0..len]);
}

#[derive(Clone)]
pub struct NewUtsName {
    sys_name: [u8; NewUtsName::MAXLEN],
    node_name: [u8; NewUtsName::MAXLEN],
    release: [u8; NewUtsName::MAXLEN],
    version: [u8; NewUtsName::MAXLEN],
    machine: [u8; NewUtsName::MAXLEN],
    domain_name: [u8; NewUtsName::MAXLEN],
}

impl NewUtsName {
    pub const MAXLEN: usize = 65;
    fn validate_len(len: usize) -> bool {
        len < Self::MAXLEN
    }
}

impl Default for NewUtsName {
    fn default() -> Self {
        let mut sys_name = [0; NewUtsName::MAXLEN];
        let mut node_name = [0; NewUtsName::MAXLEN];
        let mut release = [0; NewUtsName::MAXLEN];
        let mut version = [0; NewUtsName::MAXLEN];
        let mut machine = [0; NewUtsName::MAXLEN];
        let mut domain_name = [0; NewUtsName::MAXLEN];

        copy_bytes_to_array(UtsNamespace::UTS_SYSNAME.as_bytes(), &mut sys_name);
        copy_bytes_to_array(UtsNamespace::UTS_NODENAME.as_bytes(), &mut node_name);
        copy_bytes_to_array(UtsNamespace::UTS_RELEASE.as_bytes(), &mut release);
        copy_bytes_to_array(UtsNamespace::UTS_VERSION.as_bytes(), &mut version);
        copy_bytes_to_array(UtsNamespace::UTS_MACHINE.as_bytes(), &mut machine);
        copy_bytes_to_array(UtsNamespace::UTS_DOMAINNAME.as_bytes(), &mut domain_name);

        Self {
            sys_name,
            node_name,
            release,
            version,
            machine,
            domain_name,
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

impl NamespaceOps for UtsNamespace {
    fn ns_common(&self) -> &NsCommon {
        &self.ns_common
    }
}

impl UtsNamespace {
    const UTS_SYSNAME: &str = "Linux";
    const UTS_NODENAME: &str = "dragonos";
    const UTS_RELEASE: &str = get_kernel_build_info().release;
    const UTS_VERSION: &str = get_kernel_build_info().version;

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

    pub fn set_hostname(&self, hostname: &[u8]) -> Result<(), SystemError> {
        // 验证长度
        if !NewUtsName::validate_len(hostname.len()) {
            return Err(SystemError::ENAMETOOLONG);
        }

        // 检查权限（需要 CAP_SYS_ADMIN）
        // TODO: 实现完整的 capability 检查
        if !self.check_uts_modify_permission() {
            return Err(SystemError::EPERM);
        }
        let mut utsname = self.utsname.lock();
        copy_bytes_to_array(hostname, &mut utsname.node_name);
        Ok(())
    }

    pub fn set_domainname(&self, domainname: &[u8]) -> Result<(), SystemError> {
        // 验证长度
        if !NewUtsName::validate_len(domainname.len()) {
            return Err(SystemError::ENAMETOOLONG);
        }

        // 检查权限（需要 CAP_SYS_ADMIN）
        // TODO: 实现完整的 capability 检查
        if !self.check_uts_modify_permission() {
            return Err(SystemError::EPERM);
        }
        let mut utsname = self.utsname.lock();
        copy_bytes_to_array(domainname, &mut utsname.domain_name);
        Ok(())
    }

    /// 检查是否有权限修改 UTS 信息
    pub fn check_uts_modify_permission(&self) -> bool {
        // 检查当前进程是否具有 CAP_SYS_ADMIN 权限
        let pcb = ProcessManager::current_pcb();
        let cred = pcb.cred();
        cred.has_cap_sys_admin()
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
