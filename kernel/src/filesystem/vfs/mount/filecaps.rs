//! security.capability ownership conversion at the selected mount boundary.
//! This codec does not grant capabilities during exec.

use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    filesystem::vfs::{permission::InodeOpContext, IndexNode, Metadata},
    process::{
        cred::{ns_capable, CAPFlags},
        namespace::user_namespace::{make_kuid, map_id_up, UserNamespace},
    },
};

pub(super) const NAME: &str = "security.capability";
const REV2: u32 = 0x0200_0000;
const REV3: u32 = 0x0300_0000;
const EFFECTIVE: u32 = 1;

pub(super) fn is_acl(name: &str) -> bool {
    matches!(name, "system.posix_acl_access" | "system.posix_acl_default")
}

pub(super) struct CapabilityValue {
    data: [u8; 24],
    len: usize,
}

impl CapabilityValue {
    pub(super) fn bytes(&self) -> &[u8] {
        &self.data[..self.len]
    }

    fn decode(value: &[u8]) -> Result<Self, SystemError> {
        if value.len() != 20 && value.len() != 24 {
            return Err(SystemError::EINVAL);
        }
        let magic = u32::from_le_bytes(value[..4].try_into().unwrap());
        let revision = if value.len() == 20 { REV2 } else { REV3 };
        if magic & !EFFECTIVE != revision {
            return Err(SystemError::EINVAL);
        }
        let mut data = [0u8; 24];
        data[..value.len()].copy_from_slice(value);
        Ok(Self {
            data,
            len: value.len(),
        })
    }

    fn root(&self) -> u32 {
        if self.len == 20 {
            0
        } else {
            u32::from_le_bytes(self.data[20..24].try_into().unwrap())
        }
    }

    fn as_revision(&mut self, root: Option<u32>) {
        let effective = self.data[0] as u32 & EFFECTIVE;
        let magic = if root.is_some() { REV3 } else { REV2 } | effective;
        self.data[..4].copy_from_slice(&magic.to_le_bytes());
        self.len = if let Some(root) = root {
            self.data[20..24].copy_from_slice(&root.to_le_bytes());
            24
        } else {
            20
        };
    }
}

pub(super) fn check_setfcap(context: &InodeOpContext, raw: &Metadata) -> Result<(), SystemError> {
    let cred = context.cred.as_ref().ok_or(SystemError::EPERM)?;
    if !cred.has_capability_wrt_inode_uidgid(&context.view_metadata(raw), CAPFlags::CAP_SETFCAP) {
        return Err(SystemError::EPERM);
    }
    Ok(())
}

pub(super) fn for_storage(
    context: &InodeOpContext,
    raw: &Metadata,
    input: &[u8],
) -> Result<CapabilityValue, SystemError> {
    let mut value = CapabilityValue::decode(input)?;
    check_setfcap(context, raw)?;
    if value.len == 20
        && context.idmap.is_none()
        && ns_capable(&context.fs_userns, CAPFlags::CAP_SETFCAP)
    {
        return Ok(value);
    }
    let cred = context.cred.as_ref().ok_or(SystemError::EPERM)?;
    let view_root = make_kuid(&cred.user_ns, value.root())?.data();
    let raw_root = match &context.idmap {
        Some(idmap) => idmap.uid_from_view(&context.fs_userns, view_root),
        None => super::idmap::identity_id(view_root),
    }
    .ok_or(SystemError::EINVAL)?;
    let root = map_id_up(
        &context.fs_userns.inner.lock().uid_map,
        u32::try_from(raw_root).map_err(|_| SystemError::EINVAL)?,
    )
    .filter(|root| *root != u32::MAX)
    .ok_or(SystemError::EINVAL)?;
    value.as_revision(Some(root));
    Ok(value)
}

fn ancestor_root_owns(mut namespace: Arc<UserNamespace>, root: u32) -> bool {
    loop {
        if map_id_up(&namespace.inner.lock().uid_map, root) == Some(0) {
            return true;
        }
        match namespace.parent_ns() {
            Some(parent) => namespace = parent,
            None => return false,
        }
    }
}

fn for_view(
    context: &InodeOpContext,
    mut value: CapabilityValue,
) -> Result<CapabilityValue, SystemError> {
    let raw = make_kuid(&context.fs_userns, value.root())
        .map_err(|_| SystemError::EOVERFLOW)?
        .data();
    let root = match &context.idmap {
        Some(idmap) => idmap.uid_into_view(&context.fs_userns, raw),
        None => super::idmap::identity_id(raw),
    }
    .ok_or(SystemError::EOVERFLOW)? as u32;
    let cred = context.cred.as_ref().ok_or(SystemError::EPERM)?;
    let local = map_id_up(&cred.user_ns.inner.lock().uid_map, root);
    match local {
        Some(local) if local != 0 && local != u32::MAX => value.as_revision(Some(local)),
        _ if ancestor_root_owns(cred.user_ns.clone(), root) => value.as_revision(None),
        _ => return Err(SystemError::EOVERFLOW),
    }
    Ok(value)
}

pub(super) fn get(
    inode: &dyn IndexNode,
    context: &InodeOpContext,
    output: &mut [u8],
) -> Result<usize, SystemError> {
    let mut raw = [0u8; 24];
    let len = inode.getxattr(NAME, &mut raw).map_err(|error| {
        if error == SystemError::ERANGE {
            SystemError::EINVAL
        } else {
            error
        }
    })?;
    if len > raw.len() {
        return Err(SystemError::EINVAL);
    }
    let value = for_view(context, CapabilityValue::decode(&raw[..len])?)?;
    if !output.is_empty() {
        if output.len() < value.len {
            return Err(SystemError::ERANGE);
        }
        output[..value.len].copy_from_slice(value.bytes());
    }
    Ok(value.len)
}
