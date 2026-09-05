use alloc::{string::String, sync::Arc};

use hashbrown::HashMap;
use system_error::SystemError;

use super::{
    KernFS, KernFSChildKey, KernFSChildKeyRef, KernFSChildren, KernFSInode, KernFSInodeArgs,
    KernInodeType,
};

/// One inode re-key requested as part of a two-node kernfs rename.
///
/// All strings are owned before commit so publication never allocates.
pub struct KernFSRenameSpec {
    inode: Arc<KernFSInode>,
    new_name: String,
    symlink_target_absolute_path: Option<String>,
}

impl KernFSRenameSpec {
    pub fn new(inode: Arc<KernFSInode>, new_name: String) -> Self {
        Self {
            inode,
            new_name,
            symlink_target_absolute_path: None,
        }
    }

    pub fn with_symlink_target_absolute_path(mut self, path: String) -> Self {
        self.symlink_target_absolute_path = Some(path);
        self
    }
}

struct PreparedRenameEntry {
    parent: Arc<KernFSInode>,
    inode: Arc<KernFSInode>,
    old_key: KernFSChildKey,
    new_key: KernFSChildKey,
    new_inode_name: String,
    symlink_target_absolute_path: Option<String>,
}

impl PreparedRenameEntry {
    fn prepare(spec: KernFSRenameSpec) -> Result<Self, SystemError> {
        validate_name(&spec.new_name)?;
        if spec.symlink_target_absolute_path.is_some()
            && spec.inode.inode_type != KernInodeType::SymLink
        {
            return Err(SystemError::EINVAL);
        }

        let parent = spec.inode.parent().ok_or(SystemError::ENOENT)?;
        let old_name = spec.inode.try_name()?;
        let new_inode_name = try_copy_string(&spec.new_name)?;
        let namespace = spec.inode.namespace();
        Ok(Self {
            parent,
            inode: spec.inode,
            old_key: KernFSChildKey::new(old_name, namespace),
            new_key: KernFSChildKey::new(spec.new_name, namespace),
            new_inode_name,
            symlink_target_absolute_path: spec.symlink_target_absolute_path,
        })
    }

    fn parent_id(&self) -> u64 {
        self.parent.inner.read().metadata.inode_id.data() as u64
    }

    fn validate(
        &self,
        children: &KernFSChildren,
        lazy: &HashMap<String, fn() -> KernFSInodeArgs>,
    ) -> Result<(), SystemError> {
        let current = children
            .get(&KernFSChildKeyRef::new(
                &self.old_key.name,
                self.old_key.namespace,
            ))
            .ok_or(SystemError::ESTALE)?;
        if !Arc::ptr_eq(current, &self.inode)
            || !self.inode.with_name(|name| name == self.old_key.name)
        {
            return Err(SystemError::ESTALE);
        }
        if self.new_key != self.old_key
            && (children.contains_key(&KernFSChildKeyRef::new(
                &self.new_key.name,
                self.new_key.namespace,
            )) || lazy.contains_key(&self.new_key.name))
        {
            return Err(SystemError::EEXIST);
        }
        Ok(())
    }

    fn publish(self, children: &mut KernFSChildren) {
        children
            .rekey(
                &KernFSChildKeyRef::new(&self.old_key.name, self.old_key.namespace),
                self.new_key,
            )
            .expect("prepared kernfs rename source disappeared under mutation gate");

        let mut inner = self.inode.inner.write();
        inner.name = self.new_inode_name;
        if let Some(path) = self.symlink_target_absolute_path {
            inner.symlink_target_absolute_path = Some(path);
        }
    }
}

/// A bounded transaction that re-keys two existing kernfs inodes without
/// replacing either inode object.
pub struct PreparedKernFSRename {
    entries: [PreparedRenameEntry; 2],
}

impl PreparedKernFSRename {
    pub fn prepare(first: KernFSRenameSpec, second: KernFSRenameSpec) -> Result<Self, SystemError> {
        if Arc::ptr_eq(&first.inode, &second.inode) {
            return Err(SystemError::EINVAL);
        }
        let mut entries = [
            PreparedRenameEntry::prepare(first)?,
            PreparedRenameEntry::prepare(second)?,
        ];
        if entries[1].parent_id() < entries[0].parent_id() {
            entries.swap(0, 1);
        }
        Ok(Self { entries })
    }

    /// Revalidates both parent maps before changing either one. Publication is
    /// allocation-free because each re-key removes and inserts one entry in the
    /// same existing namespace bucket.
    pub fn commit(self) -> Result<(), SystemError> {
        let [first, second] = self.entries;
        if Arc::ptr_eq(&first.parent, &second.parent) {
            return commit_same_parent(first, second);
        }

        let first_parent = first.parent.clone();
        let second_parent = second.parent.clone();
        let _first_gate = first_parent.child_mutation.lock();
        let _second_gate = second_parent.child_mutation.lock();
        let mut first_children = first_parent.children.lock();
        let mut second_children = second_parent.children.lock();
        let first_lazy = first_parent.lazy_list.lock();
        let second_lazy = second_parent.lazy_list.lock();

        first.validate(&first_children, &first_lazy)?;
        second.validate(&second_children, &second_lazy)?;
        drop(first_lazy);
        drop(second_lazy);
        first.publish(&mut first_children);
        second.publish(&mut second_children);
        Ok(())
    }
}

fn commit_same_parent(
    first: PreparedRenameEntry,
    second: PreparedRenameEntry,
) -> Result<(), SystemError> {
    if first.new_key == second.new_key {
        return Err(SystemError::EEXIST);
    }
    let parent = first.parent.clone();
    let _gate = parent.child_mutation.lock();
    let mut children = parent.children.lock();
    let lazy = parent.lazy_list.lock();
    first.validate(&children, &lazy)?;
    second.validate(&children, &lazy)?;
    drop(lazy);
    first.publish(&mut children);
    second.publish(&mut children);
    Ok(())
}

fn validate_name(name: &str) -> Result<(), SystemError> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(SystemError::EINVAL);
    }
    if name.len() > KernFS::MAX_NAMELEN {
        return Err(SystemError::ENAMETOOLONG);
    }
    Ok(())
}

fn try_copy_string(source: &str) -> Result<String, SystemError> {
    let mut result = String::new();
    result
        .try_reserve_exact(source.len())
        .map_err(|_| SystemError::ENOMEM)?;
    result.push_str(source);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use alloc::{string::ToString, sync::Arc};

    use super::*;
    use crate::filesystem::vfs::InodeMode;

    fn test_tree() -> (
        Arc<KernFSInode>,
        Arc<KernFSInode>,
        Arc<KernFSInode>,
        Arc<KernFSInode>,
    ) {
        let root = KernFS::create_root_inode();
        let mode = InodeMode::from_bits_truncate(0o755);
        let devices = root
            .add_dir("devices".to_string(), mode, None, None)
            .unwrap();
        let class = root.add_dir("class".to_string(), mode, None, None).unwrap();
        let device = devices
            .add_dir("eth0".to_string(), mode, None, None)
            .unwrap();
        let link = class
            .add_link("eth0".to_string(), &device, "/sys/devices/eth0".to_string())
            .unwrap();
        (devices, class, device, link)
    }

    #[test]
    fn two_parent_rename_preserves_inode_identity_and_updates_link_path() {
        let (devices, class, device, link) = test_tree();
        let plan = PreparedKernFSRename::prepare(
            KernFSRenameSpec::new(device.clone(), "eth1".to_string()),
            KernFSRenameSpec::new(link.clone(), "eth1".to_string())
                .with_symlink_target_absolute_path("/sys/devices/eth1".to_string()),
        )
        .unwrap();

        plan.commit().unwrap();

        assert!(!devices
            .children
            .lock()
            .contains_key(&KernFSChildKeyRef::new("eth0", None)));
        assert!(!class
            .children
            .lock()
            .contains_key(&KernFSChildKeyRef::new("eth0", None)));
        assert!(Arc::ptr_eq(
            devices
                .children
                .lock()
                .get(&KernFSChildKeyRef::new("eth1", None))
                .unwrap(),
            &device
        ));
        assert!(Arc::ptr_eq(
            class
                .children
                .lock()
                .get(&KernFSChildKeyRef::new("eth1", None))
                .unwrap(),
            &link
        ));
        assert_eq!(device.name(), "eth1");
        assert_eq!(link.name(), "eth1");
        assert_eq!(
            link.inner.read().symlink_target_absolute_path.as_deref(),
            Some("/sys/devices/eth1")
        );
    }

    #[test]
    fn conflict_leaves_both_names_and_link_path_unchanged() {
        let (devices, class, device, link) = test_tree();
        class
            .add_dir(
                "eth1".to_string(),
                InodeMode::from_bits_truncate(0o755),
                None,
                None,
            )
            .unwrap();
        let plan = PreparedKernFSRename::prepare(
            KernFSRenameSpec::new(device.clone(), "eth1".to_string()),
            KernFSRenameSpec::new(link.clone(), "eth1".to_string())
                .with_symlink_target_absolute_path("/sys/devices/eth1".to_string()),
        )
        .unwrap();

        assert_eq!(plan.commit(), Err(SystemError::EEXIST));

        assert!(Arc::ptr_eq(
            devices
                .children
                .lock()
                .get(&KernFSChildKeyRef::new("eth0", None))
                .unwrap(),
            &device
        ));
        assert!(Arc::ptr_eq(
            class
                .children
                .lock()
                .get(&KernFSChildKeyRef::new("eth0", None))
                .unwrap(),
            &link
        ));
        assert!(!devices
            .children
            .lock()
            .contains_key(&KernFSChildKeyRef::new("eth1", None)));
        assert_eq!(device.name(), "eth0");
        assert_eq!(link.name(), "eth0");
        assert_eq!(
            link.inner.read().symlink_target_absolute_path.as_deref(),
            Some("/sys/devices/eth0")
        );
    }

    #[test]
    fn namespace_keys_allow_same_name_and_rename_only_one_view() {
        let root = KernFS::create_root_inode();
        let mode = InodeMode::from_bits_truncate(0o755);
        let devices = root
            .add_dir("devices".to_string(), mode, None, None)
            .unwrap();
        let class = root.add_dir("class".to_string(), mode, None, None).unwrap();
        devices.enable_namespace_children().unwrap();
        class.enable_namespace_children().unwrap();
        let tag_a = super::super::KernFSNamespaceTag::new(11);
        let tag_b = super::super::KernFSNamespaceTag::new(12);
        let device_a = devices
            .add_dir_ns("lo".to_string(), mode, None, None, tag_a)
            .unwrap();
        let device_b = devices
            .add_dir_ns("lo".to_string(), mode, None, None, tag_b)
            .unwrap();
        let link_a = class
            .add_link("lo".to_string(), &device_a, "/sys/devices/lo".to_string())
            .unwrap();
        class
            .add_link("lo".to_string(), &device_b, "/sys/devices/lo".to_string())
            .unwrap();

        let plan = PreparedKernFSRename::prepare(
            KernFSRenameSpec::new(device_a.clone(), "lan0".to_string()),
            KernFSRenameSpec::new(link_a, "lan0".to_string())
                .with_symlink_target_absolute_path("/sys/devices/lan0".to_string()),
        )
        .unwrap();
        plan.commit().unwrap();

        assert!(devices.find_ns("lo", tag_a).is_err());
        assert!(Arc::ptr_eq(
            &devices
                .find_ns("lan0", tag_a)
                .unwrap()
                .downcast_arc::<KernFSInode>()
                .unwrap(),
            &device_a
        ));
        assert!(Arc::ptr_eq(
            &devices
                .find_ns("lo", tag_b)
                .unwrap()
                .downcast_arc::<KernFSInode>()
                .unwrap(),
            &device_b
        ));
        assert_eq!(
            class.list_ns(tag_a).unwrap(),
            vec![".".to_string(), "..".to_string(), "lan0".to_string()]
        );
        assert_eq!(
            class.list_ns(tag_b).unwrap(),
            vec![".".to_string(), "..".to_string(), "lo".to_string()]
        );
    }
}
