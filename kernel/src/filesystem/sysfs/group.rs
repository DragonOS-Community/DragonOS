use core::intrinsics::unlikely;

use alloc::{string::ToString, sync::Arc};
use log::{error, warn};
use system_error::SystemError;

use crate::{
    driver::base::kobject::KObject,
    filesystem::{
        kernfs::{callback::KernInodePrivateData, KernFSInode},
        sysfs::{dir::SysKernDirPriv, sysfs_instance, SysFSKernPrivateData},
        vfs::{syscall::ModeType, IndexNode},
    },
    libs::casting::DowncastArc,
};

use super::{AttributeGroup, SysFS};

impl SysFS {
    /// 在sysfs中，为指定的kobject的属性组创建文件夹
    pub fn create_groups(
        &self,
        kobj: &Arc<dyn KObject>,
        groups: &[&'static dyn AttributeGroup],
    ) -> Result<(), SystemError> {
        return self.do_create_groups(kobj, groups, false);
    }

    fn do_create_groups(
        &self,
        kobj: &Arc<dyn KObject>,
        groups: &[&'static dyn AttributeGroup],
        update: bool,
    ) -> Result<(), SystemError> {
        for i in 0..groups.len() {
            let group = groups[i];
            if group.attrs().is_empty() {
                continue;
            }
            if let Err(e) = self.do_create_group(kobj, group, update) {
                error!(
                    "Failed to create group '{}', err={e:?}",
                    group.name().unwrap_or("")
                );
                for j in (0..=i).rev() {
                    self.remove_group(kobj, groups[j]).ok();
                }
                return Err(e);
            }
        }
        return Ok(());
    }

    fn do_create_group(
        &self,
        kobj: &Arc<dyn KObject>,
        group: &'static dyn AttributeGroup,
        update: bool,
    ) -> Result<(), SystemError> {
        // kobj的inode必须存在
        let kobj_inode = kobj.inode().ok_or(SystemError::EINVAL)?;

        if group.attrs().is_empty() {
            return Err(SystemError::EINVAL);
        }

        let parent_inode: Arc<KernFSInode>;
        if group.name().is_some() {
            if update {
                // 如果是更新，那么group的name必须存在
                parent_inode = kobj_inode
                    .find(group.name().unwrap())
                    .map_err(|_| SystemError::EINVAL)?
                    .downcast_arc()
                    .unwrap();
            } else {
                let private_data = KernInodePrivateData::SysFS(SysFSKernPrivateData::Dir(
                    SysKernDirPriv::new(kobj.clone()),
                ));
                parent_inode = kobj_inode
                    .add_dir(
                        group.name().unwrap().to_string(),
                        ModeType::S_IRWXU | ModeType::S_IRUGO | ModeType::S_IXUGO,
                        Some(private_data),
                        None,
                    )
                    .map_err(|e| {
                        if e == SystemError::EEXIST {
                            self.warn_duplicate(&kobj_inode, group.name().unwrap());
                        }
                        e
                    })?;
            }
        } else {
            parent_inode = kobj_inode.clone();
        }

        if let Err(e) = self.group_create_files(parent_inode.clone(), kobj, group, update) {
            if group.name().is_some() {
                parent_inode.remove_recursive();
            }
            return Err(e);
        }

        return Ok(());
    }

    pub fn remove_groups(
        &self,
        kobj: &Arc<dyn KObject>,
        groups: &'static [&'static dyn AttributeGroup],
    ) {
        for group in groups.iter() {
            self.remove_group(kobj, *group).ok();
        }
    }

    /// 从一个kobject中移除一个group
    ///
    /// This function removes a group of attributes from a kobject.  The attributes
    /// previously have to have been created for this group, otherwise it will fail.
    ///
    /// ## 参数
    ///
    /// - `kobj` - 要移除group的kobject
    /// - `group` - 要移除的group
    ///
    ///
    pub fn remove_group(
        &self,
        kobj: &Arc<dyn KObject>,
        group: &'static dyn AttributeGroup,
    ) -> Result<(), SystemError> {
        let inode = kobj.inode().unwrap();
        let parent_inode: Arc<KernFSInode>;
        if let Some(name) = group.name() {
            parent_inode = inode
                .find(name)
                .inspect_err(|_e| {
                    warn!("sysfs group '{name}' not found for kobject {kobj:?}");
                })?
                .downcast_arc()
                .unwrap();
        } else {
            parent_inode = inode;
        }

        self.group_remove_files(&parent_inode, group);

        if group.name().is_some() {
            parent_inode.remove_recursive();
        }

        return Ok(());
    }

    /// 创建属性组的文件
    ///
    /// ## 参数
    ///
    /// - `parent` - 属性组的父文件夹
    /// - `kobj` - 属性组所属的kobject
    /// - `group` - 属性组
    /// - `update` - 当前是否正在更新属性
    ///
    /// https://code.dragonos.org.cn/xref/linux-6.1.9/fs/sysfs/group.c#34
    fn group_create_files(
        &self,
        parent: Arc<KernFSInode>,
        kobj: &Arc<dyn KObject>,
        group: &'static dyn AttributeGroup,
        update: bool,
    ) -> Result<(), SystemError> {
        let mut e = Ok(());
        for attr in group.attrs() {
            let mut mode = attr.mode();

            // 由于我们在更新的时候，可能会更改visibility和permissions，所以需要先删除再创建
            if update {
                parent.remove(attr.name()).ok();
            }
            if let Some(mt) = group.is_visible(kobj.clone(), *attr) {
                mode = mt;
                // 当前属性不可见，跳过
                if mode.is_empty() {
                    continue;
                }
            }

            if unlikely((mode.bits() & (!0o644)) != 0) {
                warn!(
                    "Attribute '{name}' has invalid mode 0{mode:o}",
                    name = attr.name(),
                    mode = mode
                );
            }

            mode = ModeType::from_bits_truncate(mode.bits() & 0o644);
            e = sysfs_instance().add_file_with_mode(&parent, *attr, mode);
            if e.is_err() {
                break;
            }
        }

        if let Err(e) = e {
            error!(
                "Failed to create sysfs files for group '{}', err={e:?}",
                group.name().unwrap_or("")
            );
            self.group_remove_files(&parent, group);
            return Err(e);
        }

        return Ok(());
    }

    fn group_remove_files(&self, _parent: &Arc<KernFSInode>, _group: &'static dyn AttributeGroup) {
        todo!("group_remove_files")
    }
}
