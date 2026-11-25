//! Namespace and per-interface IPv6 sysctls.

use crate::filesystem::procfs::template::ProcWriteCallback;
use crate::filesystem::procfs::Builder;
use crate::filesystem::{
    procfs::{
        template::{DirOps, FileOps, ProcDir, ProcDirBuilder, ProcFileBuilder},
        utils::proc_read,
    },
    vfs::{file::FileFlags, FilePrivateData, IndexNode, InodeMode, PrimaryWriteResult},
};
use crate::libs::mutex::MutexGuard;
use crate::net::{
    address::ipv6_autoconf::{self, AddressSetting, PrefixSetting},
    rtnl,
};
use alloc::{
    format,
    string::ToString,
    sync::{Arc, Weak},
};
use core::sync::atomic::{AtomicI32, Ordering};
use system_error::SystemError;

use crate::process::namespace::net_namespace::NetNamespace;
use crate::time::clocksource::HZ;

use super::super::numeric::parse_numeric_sysctl;
use super::device_conf::{DeviceConfFamily, DeviceDirOps};
use super::ipv6_stable_secret;
use super::namespace::SysctlNamespace;

#[derive(Debug)]
pub(super) struct Ipv6DirOps(SysctlNamespace);

#[derive(Debug)]
struct ConfDirOps(SysctlNamespace);

#[derive(Debug, Clone, Copy)]
enum NamespaceConfScope {
    All,
    Default,
}

impl NamespaceConfScope {
    fn from_dir_name(name: &str) -> Option<Self> {
        match name {
            "all" => Some(Self::All),
            "default" => Some(Self::Default),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct NamespaceConfDirOps {
    namespace: SysctlNamespace,
    target: NamespaceConfScope,
}

impl Ipv6DirOps {
    pub(super) fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(
            Self(SysctlNamespace::current()),
            InodeMode::from_bits_truncate(0o555),
        )
        .parent(parent)
        .build()
        .unwrap()
    }

    pub(super) fn is_current(&self) -> bool {
        self.0.is_current()
    }
}

impl ConfDirOps {
    fn new_inode(parent: Weak<dyn IndexNode>, namespace: SysctlNamespace) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(Self(namespace), InodeMode::from_bits_truncate(0o555))
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl NamespaceConfDirOps {
    fn new_inode(
        parent: Weak<dyn IndexNode>,
        namespace: SysctlNamespace,
        target: NamespaceConfScope,
    ) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(
            Self { namespace, target },
            InodeMode::from_bits_truncate(0o555),
        )
        .parent(parent)
        .build()
        .unwrap()
    }
}

impl DirOps for Ipv6DirOps {
    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.0.upgrade()?;
        let field = match name {
            "conf" => None,
            "idgen_delay" => Some(IdGenField::Delay),
            "idgen_retries" => Some(IdGenField::Retries),
            _ => return Err(SystemError::ENOENT),
        };
        let mut children = dir.cached_children().write();
        Ok(children
            .entry(name.to_string())
            .or_insert_with(|| {
                let parent = dir.self_ref_weak().clone();
                if let Some(field) = field {
                    ProcFileBuilder::new(
                        IdGenFileOps {
                            namespace: self.0.clone(),
                            field,
                        },
                        InodeMode::from_bits_truncate(0o644),
                    )
                    .parent(parent)
                    .sysctl_permissions()
                    .build()
                    .unwrap()
                } else {
                    ConfDirOps::new_inode(parent, self.0.clone())
                }
            })
            .clone())
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        for name in ["conf", "idgen_delay", "idgen_retries"] {
            let _ = self.lookup_child(dir, name);
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum IdGenField {
    Delay,
    Retries,
}

impl IdGenField {
    fn cell(self, netns: &NetNamespace) -> &AtomicI32 {
        match self {
            Self::Delay => &netns.ipv6_idgen_delay,
            Self::Retries => &netns.ipv6_idgen_retries,
        }
    }
}

#[derive(Debug)]
struct IdGenFileOps {
    namespace: SysctlNamespace,
    field: IdGenField,
}

impl FileOps for IdGenFileOps {
    const SEEK_KIND: crate::filesystem::procfs::template::ProcfsSeekKind =
        crate::filesystem::procfs::template::ProcfsSeekKind::Default;

    fn validate_inode(&self) -> Result<(), SystemError> {
        self.namespace.upgrade().map(|_| ())
    }

    fn sysctl_access_bits(&self) -> Result<Option<u32>, SystemError> {
        self.namespace.access_bits().map(Some)
    }

    fn owner(&self) -> Option<(usize, usize)> {
        self.namespace.owner()
    }

    fn supports_write_iter_flags(&self) -> bool {
        true
    }

    fn write_callback(&self) -> Option<ProcWriteCallback<Self>> {
        Some(|ops, offset, _, buf, _data| {
            let netns = ops.namespace.upgrade()?;
            if SysctlNamespace::access_bits_for(&netns) & 2 == 0 {
                return Err(SystemError::EPERM);
            }
            if buf.is_empty() || offset != 0 {
                return Ok(PrimaryWriteResult::new(buf.len()));
            }
            let (value, consumed) = parse_numeric_sysctl(buf)?;
            let value = i32::try_from(value).map_err(|_| SystemError::EINVAL)?;
            if matches!(ops.field, IdGenField::Delay)
                && value.unsigned_abs() > i32::MAX as u32 / HZ as u32
            {
                return Err(SystemError::EINVAL);
            }
            ops.field.cell(&netns).store(value, Ordering::Release);
            Ok(PrimaryWriteResult::new(consumed))
        })
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let netns = self.namespace.upgrade()?;
        if offset != 0 {
            return Ok(0);
        }
        let value = self.field.cell(&netns).load(Ordering::Acquire);
        proc_read(0, len, buf, format!("{value}\n").as_bytes())
    }
}

impl DirOps for ConfDirOps {
    fn validate_inode(&self) -> Result<(), SystemError> {
        self.0.upgrade().map(|_| ())
    }

    fn before_read_dir(&self) -> Result<(), SystemError> {
        self.validate_inode()
    }

    fn validate_child(&self, child: &dyn IndexNode) -> bool {
        DeviceDirOps::valid_child(child)
    }

    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.0.upgrade()?;
        if let Some(target) = NamespaceConfScope::from_dir_name(name) {
            return Ok(dir
                .cached_children()
                .write()
                .entry(name.to_string())
                .or_insert_with(|| {
                    NamespaceConfDirOps::new_inode(
                        dir.self_ref_weak().clone(),
                        self.0.clone(),
                        target,
                    )
                })
                .clone());
        }
        DeviceDirOps::lookup(dir, &self.0, DeviceConfFamily::Ipv6, name)
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        for name in ["all", "default"] {
            let _ = self.lookup_child(dir, name);
        }
        DeviceDirOps::populate(dir, &self.0, DeviceConfFamily::Ipv6);
    }
}

impl DirOps for NamespaceConfDirOps {
    fn validate_inode(&self) -> Result<(), SystemError> {
        self.namespace.upgrade().map(|_| ())
    }

    fn before_read_dir(&self) -> Result<(), SystemError> {
        self.validate_inode()
    }

    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.validate_inode()?;
        let field = match name {
            "forwarding" => NamespaceConfField::Forwarding,
            "accept_ra" => NamespaceConfField::AcceptRa,
            "accept_ra_pinfo" => NamespaceConfField::Prefix(PrefixSetting::AcceptRaPinfo),
            "autoconf" => NamespaceConfField::Prefix(PrefixSetting::Autoconf),
            "keep_addr_on_down" => NamespaceConfField::KeepAddrOnDown,
            "stable_secret" => NamespaceConfField::StableSecret,
            "addr_gen_mode" => NamespaceConfField::Address(AddressSetting::AddrGenMode),
            "accept_dad" => NamespaceConfField::Address(AddressSetting::AcceptDad),
            "dad_transmits" => NamespaceConfField::Address(AddressSetting::DadTransmits),
            _ => return Err(SystemError::ENOENT),
        };
        let mode = field.mode();
        Ok(dir
            .cached_children()
            .write()
            .entry(name.to_string())
            .or_insert_with(|| {
                ProcFileBuilder::new(
                    NamespaceConfFileOps {
                        namespace: self.namespace.clone(),
                        target: self.target,
                        field,
                    },
                    InodeMode::from_bits_truncate(mode),
                )
                .parent(dir.self_ref_weak().clone())
                .sysctl_permissions()
                .build()
                .unwrap()
            })
            .clone())
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        let _ = self.lookup_child(dir, "forwarding");
        let _ = self.lookup_child(dir, "accept_ra");
        let _ = self.lookup_child(dir, "accept_ra_pinfo");
        let _ = self.lookup_child(dir, "autoconf");
        let _ = self.lookup_child(dir, "keep_addr_on_down");
        let _ = self.lookup_child(dir, "addr_gen_mode");
        let _ = self.lookup_child(dir, "accept_dad");
        let _ = self.lookup_child(dir, "dad_transmits");
        let _ = self.lookup_child(dir, "stable_secret");
    }
}

#[derive(Debug)]
enum NamespaceConfField {
    Forwarding,
    AcceptRa,
    KeepAddrOnDown,
    Prefix(PrefixSetting),
    Address(AddressSetting),
    StableSecret,
}

impl NamespaceConfField {
    fn mode(&self) -> u32 {
        if matches!(self, Self::StableSecret) {
            ipv6_stable_secret::MODE
        } else {
            0o644
        }
    }
}

#[derive(Debug)]
struct NamespaceConfFileOps {
    namespace: SysctlNamespace,
    target: NamespaceConfScope,
    field: NamespaceConfField,
}

impl FileOps for NamespaceConfFileOps {
    const SEEK_KIND: crate::filesystem::procfs::template::ProcfsSeekKind =
        crate::filesystem::procfs::template::ProcfsSeekKind::Default;

    fn validate_inode(&self) -> Result<(), SystemError> {
        self.namespace.upgrade().map(|_| ())
    }

    fn open(
        &self,
        _data: &mut MutexGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), SystemError> {
        self.validate_inode()
    }

    fn sysctl_access_bits(&self) -> Result<Option<u32>, SystemError> {
        let netns = self.namespace.upgrade()?;
        Ok(Some(SysctlNamespace::access_bits_for_mode(
            &netns,
            self.field.mode(),
        )))
    }

    fn owner(&self) -> Option<(usize, usize)> {
        self.namespace.owner()
    }

    fn write_callback(&self) -> Option<ProcWriteCallback<Self>> {
        Some(|ops, offset, _, buf, _data| {
            let netns = ops.namespace.upgrade()?;
            if SysctlNamespace::access_bits_for_mode(&netns, ops.field.mode()) & 2 == 0 {
                return Err(SystemError::EPERM);
            }
            if matches!(ops.field, NamespaceConfField::StableSecret) {
                if matches!(ops.target, NamespaceConfScope::All) {
                    return Err(SystemError::EIO);
                }
                let rtnl = rtnl::lock();
                let current = ipv6_autoconf::default_stable_secret(&netns);
                if let Some(secret) = ipv6_stable_secret::parse_write(current, offset, buf)? {
                    ipv6_autoconf::set_default_stable_secret(&rtnl, &netns, secret);
                }
                return Ok(PrimaryWriteResult::new(buf.len()));
            }
            if buf.is_empty() || offset != 0 {
                return Ok(PrimaryWriteResult::new(buf.len()));
            }
            let (value, consumed) = parse_numeric_sysctl(buf)?;
            let value = i32::try_from(value).map_err(|_| SystemError::EINVAL)?;
            let rtnl = rtnl::lock();
            match (&ops.field, ops.target) {
                (NamespaceConfField::StableSecret, _) => unreachable!(),
                (NamespaceConfField::Forwarding, NamespaceConfScope::All) => {
                    crate::net::route::ra::set_forwarding(&rtnl, &netns, None, value)?;
                }
                (NamespaceConfField::Forwarding, NamespaceConfScope::Default) => {
                    netns.set_ipv6_default_forwarding(&rtnl, value);
                }
                (NamespaceConfField::AcceptRa, NamespaceConfScope::All) => {
                    netns.set_ipv6_all_accept_ra(&rtnl, value);
                }
                (NamespaceConfField::AcceptRa, NamespaceConfScope::Default) => {
                    netns.set_ipv6_default_accept_ra(&rtnl, value);
                }
                (NamespaceConfField::KeepAddrOnDown, NamespaceConfScope::All) => {
                    netns.set_ipv6_all_keep_addr_on_down(&rtnl, value);
                }
                (NamespaceConfField::KeepAddrOnDown, NamespaceConfScope::Default) => {
                    netns.set_ipv6_default_keep_addr_on_down(&rtnl, value);
                }
                (NamespaceConfField::Address(setting), scope) => {
                    setting.write_scope(
                        &rtnl,
                        &netns,
                        matches!(scope, NamespaceConfScope::All),
                        value,
                    )?;
                }
                (NamespaceConfField::Prefix(setting), NamespaceConfScope::All) => {
                    netns.ipv6_all_prefixes.lock().set(*setting, value);
                }
                (NamespaceConfField::Prefix(setting), NamespaceConfScope::Default) => {
                    netns.ipv6_default_prefixes.lock().set(*setting, value);
                }
            }
            Ok(PrimaryWriteResult::new(consumed))
        })
    }

    fn supports_write_iter_flags(&self) -> bool {
        true
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let netns = self.namespace.upgrade()?;
        if matches!(self.field, NamespaceConfField::StableSecret) {
            if SysctlNamespace::access_bits_for_mode(&netns, self.field.mode()) & 4 == 0 {
                return Err(SystemError::EPERM);
            }
            if matches!(self.target, NamespaceConfScope::All) {
                return Err(SystemError::EIO);
            }
            return ipv6_stable_secret::read(
                ipv6_autoconf::default_stable_secret(&netns),
                offset,
                len,
                buf,
            );
        }
        if offset != 0 {
            return Ok(0);
        }
        let value = match (&self.field, self.target) {
            (NamespaceConfField::StableSecret, _) => unreachable!(),
            (NamespaceConfField::Forwarding, NamespaceConfScope::All) => {
                netns.ipv6_forwarding_value()
            }
            (NamespaceConfField::Forwarding, NamespaceConfScope::Default) => {
                netns.ipv6_default_forwarding()
            }
            (NamespaceConfField::AcceptRa, NamespaceConfScope::All) => netns.ipv6_all_accept_ra(),
            (NamespaceConfField::AcceptRa, NamespaceConfScope::Default) => {
                netns.ipv6_default_accept_ra()
            }
            (NamespaceConfField::KeepAddrOnDown, NamespaceConfScope::All) => {
                netns.ipv6_all_keep_addr_on_down()
            }
            (NamespaceConfField::KeepAddrOnDown, NamespaceConfScope::Default) => {
                netns.ipv6_default_keep_addr_on_down()
            }
            (NamespaceConfField::Address(setting), scope) => {
                setting.read_scope(&netns, matches!(scope, NamespaceConfScope::All))
            }
            (NamespaceConfField::Prefix(setting), NamespaceConfScope::All) => {
                netns.ipv6_all_prefixes.lock().get(*setting)
            }
            (NamespaceConfField::Prefix(setting), NamespaceConfScope::Default) => {
                netns.ipv6_default_prefixes.lock().get(*setting)
            }
        };
        proc_read(0, len, buf, format!("{value}\n").as_bytes())
    }
}
