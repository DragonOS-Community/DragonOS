//! A per-device sysctl inode tracks one registration under one name.

use alloc::{
    format,
    string::ToString,
    sync::{Arc, Weak},
};
use system_error::SystemError;

use crate::{
    driver::net::Iface,
    filesystem::{
        procfs::{
            template::{
                Builder, DirOps, FileOps, ProcDir, ProcDirBuilder, ProcFileBuilder,
                ProcWriteCallback,
            },
            utils::proc_read,
        },
        vfs::{file::FileFlags, FilePrivateData, IndexNode, InodeMode, PrimaryWriteResult},
    },
    libs::mutex::MutexGuard,
    net::{
        address::{
            ipv6_autoconf::{PrefixSetting, Setting},
            set_device_ipv6_disabled,
        },
        rtnl,
    },
    process::namespace::net_namespace::NetNamespace,
};

use super::{super::numeric::parse_numeric_sysctl, namespace::SysctlNamespace};

#[derive(Debug, Clone, Copy)]
pub(super) enum DeviceConfFamily {
    Ipv4,
    Ipv6,
}

impl DeviceConfFamily {
    fn entries(self) -> &'static [&'static str] {
        match self {
            Self::Ipv4 => &["route_localnet"],
            Self::Ipv6 => &[
                "accept_dad",
                "accept_ra",
                "accept_ra_pinfo",
                "addr_gen_mode",
                "autoconf",
                "dad_transmits",
                "disable_ipv6",
                "forwarding",
                "keep_addr_on_down",
                "router_solicitations",
            ],
        }
    }
}

#[derive(Debug, Clone)]
struct DeviceIdentity {
    namespace: SysctlNamespace,
    iface: Weak<dyn Iface>,
    namespace_epoch: u64,
    name_generation: u64,
    ipv6_device_generation: Option<u64>,
}

impl DeviceIdentity {
    fn new(
        namespace: SysctlNamespace,
        iface: &Arc<dyn Iface>,
        family: DeviceConfFamily,
    ) -> Option<Self> {
        let ipv6_device_generation = match family {
            DeviceConfFamily::Ipv4 => None,
            DeviceConfFamily::Ipv6 => Some(iface.common().ipv6_device_generation()?),
        };
        Some(Self {
            namespace,
            iface: Arc::downgrade(iface),
            namespace_epoch: iface.common().namespace_epoch(),
            name_generation: iface.common().name_generation(),
            ipv6_device_generation,
        })
    }

    fn resolve(&self) -> Result<(Arc<NetNamespace>, Arc<dyn Iface>), SystemError> {
        let netns = self.namespace.upgrade()?;
        let iface = self.iface.upgrade().ok_or(SystemError::ENOENT)?;
        if self.namespace_epoch & 1 != 0
            || iface.common().namespace_epoch() != self.namespace_epoch
            || iface.common().name_generation() != self.name_generation
            || self.ipv6_device_generation.is_some_and(|generation| {
                iface.common().ipv6_device_generation() != Some(generation)
            })
            || !iface
                .net_namespace()
                .is_some_and(|owner| Arc::ptr_eq(&owner, &netns))
        {
            return Err(SystemError::ENOENT);
        }
        Ok((netns, iface))
    }
}

#[derive(Debug)]
pub(super) struct DeviceDirOps {
    identity: DeviceIdentity,
    family: DeviceConfFamily,
}

impl DeviceDirOps {
    pub(super) fn lookup<T: DirOps + 'static>(
        dir: &ProcDir<T>,
        namespace: &SysctlNamespace,
        family: DeviceConfFamily,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let netns = namespace.upgrade()?;
        for iface in netns.device_list().values() {
            let child = iface.common().with_iface_name(|current_name| {
                if current_name == name {
                    Self::cache(dir, namespace, family, name, iface)
                } else {
                    None
                }
            });
            if let Some(child) = child {
                return Ok(child);
            }
        }
        Err(SystemError::ENOENT)
    }

    pub(super) fn populate<T: DirOps + 'static>(
        dir: &ProcDir<T>,
        namespace: &SysctlNamespace,
        family: DeviceConfFamily,
    ) {
        let Ok(netns) = namespace.upgrade() else {
            return;
        };
        for iface in netns.device_list().values() {
            iface.common().with_iface_name(|name| {
                if !matches!(name, "all" | "default") {
                    let _ = Self::cache(dir, namespace, family, name, iface);
                }
            });
        }
        dir.cached_children()
            .write()
            .retain(|_, child| Self::valid_child(child.as_ref()));
    }

    pub(super) fn valid_child(child: &dyn IndexNode) -> bool {
        child
            .as_any_ref()
            .downcast_ref::<ProcDir<Self>>()
            .is_none_or(|inode| inode.ops().validate_inode().is_ok())
    }

    fn cache<T: DirOps + 'static>(
        dir: &ProcDir<T>,
        namespace: &SysctlNamespace,
        family: DeviceConfFamily,
        name: &str,
        iface: &Arc<dyn Iface>,
    ) -> Option<Arc<dyn IndexNode>> {
        let mut children = dir.cached_children().write();
        let identity = DeviceIdentity::new(namespace.clone(), iface, family)?;
        if let Some(child) = children.get(name) {
            if child
                .as_any_ref()
                .downcast_ref::<ProcDir<Self>>()
                .is_some_and(|inode| inode.ops().has_identity(&identity))
            {
                return Some(child.clone());
            }
        }
        let inode = ProcDirBuilder::new(
            Self { identity, family },
            InodeMode::from_bits_truncate(0o555),
        )
        .parent(dir.self_ref_weak().clone())
        .build()
        .unwrap();
        children.insert(name.to_string(), inode.clone());
        Some(inode)
    }

    fn has_identity(&self, identity: &DeviceIdentity) -> bool {
        Weak::ptr_eq(&self.identity.iface, &identity.iface)
            && self.identity.namespace_epoch == identity.namespace_epoch
            && self.identity.name_generation == identity.name_generation
            && self.identity.ipv6_device_generation == identity.ipv6_device_generation
    }
}

impl DirOps for DeviceDirOps {
    fn validate_inode(&self) -> Result<(), SystemError> {
        self.identity.resolve().map(|_| ())
    }

    fn before_read_dir(&self) -> Result<(), SystemError> {
        self.validate_inode()
    }

    fn owner(&self) -> Option<(usize, usize)> {
        self.identity.namespace.owner()
    }

    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.validate_inode()?;
        let field = match (self.family, name) {
            (DeviceConfFamily::Ipv4, "route_localnet") => DeviceField::RouteLocalnet,
            (DeviceConfFamily::Ipv6, "disable_ipv6") => DeviceField::DisableIpv6,
            (DeviceConfFamily::Ipv6, "forwarding") => DeviceField::Ipv6Forwarding,
            (DeviceConfFamily::Ipv6, "accept_ra") => DeviceField::Ipv6AcceptRa,
            (DeviceConfFamily::Ipv6, "accept_ra_pinfo") => {
                DeviceField::Ipv6Prefix(PrefixSetting::AcceptRaPinfo)
            }
            (DeviceConfFamily::Ipv6, "autoconf") => {
                DeviceField::Ipv6Prefix(PrefixSetting::Autoconf)
            }
            (DeviceConfFamily::Ipv6, "accept_dad") => DeviceField::Ipv6Autoconf(Setting::AcceptDad),
            (DeviceConfFamily::Ipv6, "addr_gen_mode") => {
                DeviceField::Ipv6Autoconf(Setting::AddrGenMode)
            }
            (DeviceConfFamily::Ipv6, "dad_transmits") => {
                DeviceField::Ipv6Autoconf(Setting::DadTransmits)
            }
            (DeviceConfFamily::Ipv6, "keep_addr_on_down") => {
                DeviceField::Ipv6Autoconf(Setting::KeepAddrOnDown)
            }
            (DeviceConfFamily::Ipv6, "router_solicitations") => {
                DeviceField::Ipv6Autoconf(Setting::RouterSolicitations)
            }
            _ => return Err(SystemError::ENOENT),
        };
        Ok(dir
            .cached_children()
            .write()
            .entry(name.to_string())
            .or_insert_with(|| {
                ProcFileBuilder::new(
                    DeviceFileOps {
                        identity: self.identity.clone(),
                        field,
                    },
                    InodeMode::from_bits_truncate(0o644),
                )
                .parent(dir.self_ref_weak().clone())
                .sysctl_permissions()
                .build()
                .unwrap()
            })
            .clone())
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        for name in self.family.entries() {
            let _ = self.lookup_child(dir, name);
        }
    }
}

#[derive(Debug)]
enum DeviceField {
    RouteLocalnet,
    DisableIpv6,
    Ipv6Forwarding,
    Ipv6AcceptRa,
    Ipv6Autoconf(Setting),
    Ipv6Prefix(PrefixSetting),
}

#[derive(Debug)]
struct DeviceFileOps {
    identity: DeviceIdentity,
    field: DeviceField,
}

impl FileOps for DeviceFileOps {
    const SEEK_KIND: crate::filesystem::procfs::template::ProcfsSeekKind =
        crate::filesystem::procfs::template::ProcfsSeekKind::Default;

    fn validate_inode(&self) -> Result<(), SystemError> {
        self.identity.resolve().map(|_| ())
    }

    fn open(
        &self,
        _data: &mut MutexGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), SystemError> {
        self.validate_inode()
    }

    fn sysctl_access_bits(&self) -> Result<Option<u32>, SystemError> {
        let (netns, _) = self.identity.resolve()?;
        Ok(Some(SysctlNamespace::access_bits_for(&netns)))
    }

    fn owner(&self) -> Option<(usize, usize)> {
        self.identity.namespace.owner()
    }

    fn supports_write_iter_flags(&self) -> bool {
        true
    }

    fn write_callback(&self) -> Option<ProcWriteCallback<Self>> {
        Some(|ops, offset, _, buf, _data| {
            let rtnl = rtnl::lock();
            let (netns, iface) = ops.identity.resolve()?;
            if SysctlNamespace::access_bits_for(&netns) & 2 == 0 {
                return Err(SystemError::EPERM);
            }
            if buf.is_empty() || offset != 0 {
                return Ok(PrimaryWriteResult::new(buf.len()));
            }
            let (value, consumed) = parse_numeric_sysctl(buf)?;
            let value = i32::try_from(value).map_err(|_| SystemError::EINVAL)?;
            match ops.field {
                DeviceField::RouteLocalnet => iface.common().set_route_localnet(&rtnl, value),
                DeviceField::DisableIpv6 => set_device_ipv6_disabled(&rtnl, &netns, &iface, value)?,
                DeviceField::Ipv6Forwarding => {
                    crate::net::route::ra::set_forwarding(
                        &rtnl,
                        &netns,
                        Some(iface.as_ref()),
                        value,
                    )?;
                }
                DeviceField::Ipv6AcceptRa => iface.common().set_ipv6_accept_ra(&rtnl, value),
                DeviceField::Ipv6Autoconf(setting) => {
                    setting.write(&rtnl, &netns, iface.as_ref(), value)?
                }
                DeviceField::Ipv6Prefix(setting) => {
                    iface
                        .common()
                        .ipv6_autoconf
                        .lock()
                        .prefixes
                        .set(setting, value);
                }
            }
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
        let _rtnl = rtnl::lock();
        let (_, iface) = self.identity.resolve()?;
        if offset != 0 {
            return Ok(0);
        }
        let value = match self.field {
            DeviceField::RouteLocalnet => iface.common().route_localnet_value(),
            DeviceField::DisableIpv6 => iface.common().ipv6_disabled_value(),
            DeviceField::Ipv6Forwarding => iface.common().ipv6_forwarding(),
            DeviceField::Ipv6AcceptRa => iface.common().ipv6_accept_ra(),
            DeviceField::Ipv6Autoconf(setting) => setting.read(iface.common()),
            DeviceField::Ipv6Prefix(setting) => {
                iface.common().ipv6_autoconf.lock().prefixes.get(setting)
            }
        };
        proc_read(0, len, buf, format!("{value}\n").as_bytes())
    }
}
