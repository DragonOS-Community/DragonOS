#![allow(dead_code)]
use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use crate::{
    driver::base::kobject::KObject,
    filesystem::{
        kernfs::{callback::KernInodePrivateData, KernFSInode},
        vfs::syscall::ModeType,
    },
};

use super::{SysFS, SysFSKernPrivateData};

#[derive(Debug)]
pub struct SysKernDirPriv {
    /// 该目录对应的kobject
    /// use weak reference to avoid cyclic reference
    kobj: Weak<dyn KObject>,
    // attribute_group: Option<&'static dyn AttributeGroup>,
}

impl SysKernDirPriv {
    pub fn new(kobj: Arc<dyn KObject>) -> Self {
        // let attribute_group = kobj.kobj_type().map(|kobj_type| kobj_type.attribute_groups()).flatten();
        Self {
            kobj: Arc::downgrade(&kobj),
            // attribute_group,
        }
    }

    pub fn kobj(&self) -> Option<Arc<dyn KObject>> {
        self.kobj.upgrade()
    }

    // pub fn attribute_group(&self) -> Option<&'static dyn AttributeGroup> {
    //     self.attribute_group
    // }
}

impl SysFS {
    /// 在sysfs中创建一个目录
    ///
    /// 如果kobj的parent为None，则会在根目录下创建一个目录。
    ///
    /// ## 参数
    ///
    /// - `kobj`: 要创建的目录对应的kobject
    ///
    /// ## 返回
    ///
    /// 返回创建的目录对应的inode
    pub fn create_dir(&self, kobj: Arc<dyn KObject>) -> Result<Arc<KernFSInode>, SystemError> {
        // 如果kobj的parent为None，则会在/sys目录下创建一个目录。
        let parent = kobj
            .parent()
            .map(|p| p.upgrade().unwrap().inode())
            .unwrap_or_else(|| Some(self.root_inode.clone()))
            .ok_or(SystemError::ENOENT)?;

        let sysfs_dir_priv = SysFSKernPrivateData::Dir(SysKernDirPriv::new(kobj.clone()));
        // 在kernfs里面创建一个目录
        let dir: Arc<KernFSInode> = parent.add_dir(
            kobj.name(),
            ModeType::from_bits_truncate(0o755),
            Some(KernInodePrivateData::SysFS(sysfs_dir_priv)),
            None,
        )?;

        kobj.set_inode(Some(dir.clone()));

        return Ok(dir);
    }

    /// 获取指定的kernfs inode在sysfs中的路径（不包含`/sys`）
    ///
    /// ## 参数
    ///
    /// - `parent`: inode的父目录
    /// - `name`: inode的名称
    ///
    /// ## 返回
    ///
    /// 返回inode在sysfs中的路径
    pub(super) fn kernfs_path(&self, parent: &Arc<KernFSInode>) -> String {
        let mut p = parent.clone();
        let mut parts = Vec::new();
        let sys_root_inode = self.root_inode();
        let mut not_reach_sys_root = false;
        while !Arc::ptr_eq(&p, sys_root_inode) {
            parts.push(p.name().to_string());
            if let Some(parent) = p.parent() {
                p = parent;
            } else {
                not_reach_sys_root = true;
                break;
            }
        }

        let mut path = String::new();
        if not_reach_sys_root {
            path.push_str("(null)");
        };

        for part in parts.iter().rev() {
            path.push('/');
            path.push_str(part);
        }

        return path;
    }

    /// 从sysfs中删除一个kobject对应的目录（包括目录自身以及目录下的所有文件、文件夹）
    pub fn remove_dir(&self, kobj: &Arc<dyn KObject>) {
        let kobj_inode = kobj.inode();
        kobj.set_inode(None);

        if let Some(inode) = kobj_inode {
            let parent = inode.parent().unwrap();
            parent.remove_recursive()
        }
    }
}
