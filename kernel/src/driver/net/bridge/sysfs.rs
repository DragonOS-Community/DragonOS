use super::{BridgeEnableDevice, BridgeIface, VethInterface};
use crate::{
    driver::base::kobject::KObject,
    filesystem::{
        kernfs::{copy_name, KernFSInode, KernFSRenameSpec},
        sysfs::{
            file::sysfs_emit_str, sysfs_instance, Attribute, AttributeGroup, SysFSOpsSupport,
            SYSFS_ATTR_MODE_RO,
        },
        vfs::{IndexNode, InodeMode},
    },
    libs::casting::DowncastArc,
};
use alloc::sync::Arc;
use system_error::SystemError;

const BRIDGE_PORTS: &str = "brif";
const PORT_GROUP: &str = "brport";
const PORT_STATE: &str = "state";

pub(super) static BRIDGE_GROUPS: [&'static dyn AttributeGroup; 1] = [&BridgeAttributes];

pub(in crate::driver::net) fn add_port_index(bridge: &BridgeIface) -> Result<(), SystemError> {
    bridge.inode().ok_or(SystemError::ENODEV)?.add_dir(
        copy_name(BRIDGE_PORTS)?,
        InodeMode::from_bits_truncate(0o755),
        None,
        None,
    )?;
    Ok(())
}

pub(in crate::driver::net) fn remove_port_index(bridge: &BridgeIface) {
    if let Some(inode) = bridge.inode() {
        remove(inode.as_ref(), BRIDGE_PORTS);
    }
}

fn remove(parent: &KernFSInode, name: &str) {
    if let Err(error) = parent.remove(name) {
        if error != SystemError::ENOENT {
            log::warn!("failed to remove bridge sysfs entry '{name}': {error:?}");
        }
    }
}

#[derive(Debug, Clone)]
pub(in crate::driver::net) struct BridgePortSysfs {
    links: Arc<KernFSInode>,
    link: Arc<KernFSInode>,
    device: Arc<KernFSInode>,
    port: Arc<KernFSInode>,
}

impl BridgePortSysfs {
    pub(super) fn create(
        bridge: &BridgeIface,
        veth: &Arc<VethInterface>,
    ) -> Result<Self, SystemError> {
        let sysfs = sysfs_instance();
        let bridge_inode = bridge.inode().ok_or(SystemError::ENODEV)?;
        let device = veth.inode().ok_or(SystemError::ENODEV)?;
        if bridge_inode.namespace() != device.namespace() {
            return Err(SystemError::EINVAL);
        }
        let links = bridge_inode
            .find(BRIDGE_PORTS)?
            .downcast_arc::<KernFSInode>()
            .ok_or(SystemError::EIO)?;
        let link_name = device.try_name()?;
        let path = sysfs.child_relative_path(&links, &device, PORT_GROUP)?;
        let kobj: Arc<dyn KObject> = veth.clone();
        sysfs.create_groups(&kobj, &[&PortAttributes])?;
        let created = (|| {
            let port = device
                .find(PORT_GROUP)?
                .downcast_arc::<KernFSInode>()
                .ok_or(SystemError::EIO)?;
            let link = links.add_link(link_name, &port, path)?;
            Ok((port, link))
        })();
        match created {
            Ok((port, link)) => Ok(Self {
                links,
                link,
                device,
                port,
            }),
            Err(error) => {
                sysfs.remove_group(&kobj, &PortAttributes).ok();
                Err(error)
            }
        }
    }

    pub(in crate::driver::net) fn remove(self) {
        self.link.remove_inode_include_self();
        remove(self.port.as_ref(), PORT_STATE);
        remove(self.device.as_ref(), PORT_GROUP);
    }

    pub(in crate::driver::net) fn prepare_rename(
        &self,
        new_name: &str,
    ) -> Result<KernFSRenameSpec, SystemError> {
        let device_parent = self.device.parent().ok_or(SystemError::ENOENT)?;
        let mut target =
            sysfs_instance().child_relative_path(&self.links, &device_parent, new_name)?;
        target
            .try_reserve_exact(PORT_GROUP.len() + 1)
            .map_err(|_| SystemError::ENOMEM)?;
        target.push('/');
        target.push_str(PORT_GROUP);
        Ok(
            KernFSRenameSpec::new(self.link.clone(), copy_name(new_name)?)
                .with_symlink_target_absolute_path(target),
        )
    }
}

#[derive(Debug)]
struct BridgeAttributes;

impl AttributeGroup for BridgeAttributes {
    fn name(&self) -> Option<&str> {
        Some("bridge")
    }

    fn attrs(&self) -> &[&'static dyn Attribute] {
        &[&StpState]
    }
}

#[derive(Debug)]
struct PortAttributes;

impl AttributeGroup for PortAttributes {
    fn name(&self) -> Option<&str> {
        Some(PORT_GROUP)
    }

    fn attrs(&self) -> &[&'static dyn Attribute] {
        &[&PortState]
    }
}

#[derive(Debug)]
struct StpState;

impl Attribute for StpState {
    fn name(&self) -> &str {
        "stp_state"
    }

    fn mode(&self) -> InodeMode {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, _kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        // The bridge does not run an STP state machine.
        sysfs_emit_str(buf, "0\n")
    }
}

#[derive(Debug)]
struct PortState;

impl Attribute for PortState {
    fn name(&self) -> &str {
        PORT_STATE
    }

    fn mode(&self) -> InodeMode {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let membership = kobj
            .as_any_ref()
            .downcast_ref::<VethInterface>()
            .and_then(BridgeEnableDevice::common_bridge_data)
            .ok_or(SystemError::ENODEV)?;
        let forwarding = membership.is_forwarding().ok_or(SystemError::ENODEV)?;
        sysfs_emit_str(buf, if forwarding { "3\n" } else { "0\n" })
    }
}
