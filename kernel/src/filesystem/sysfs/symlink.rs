use alloc::{
    borrow::ToOwned,
    string::{String, ToString},
    sync::Arc,
};
use system_error::SystemError;

use crate::{driver::base::kobject::KObject, filesystem::kernfs::KernFSInode};

use super::SysFS;

impl SysFS {
    /// 在sysfs中创建一个符号链接
    ///
    /// ## 参数
    ///
    /// - `kobj`: object whose directory we're creating the link in. (符号链接所在目录)
    ///    如果为None，则创建在sysfs的根目录下
    /// - `target`: object we're pointing to.
    /// - `name`: 符号链接的名称
    ///
    /// 参考：https://code.dragonos.org.cn/xref/linux-6.1.9/fs/sysfs/symlink.c#89
    pub fn create_link(
        &self,
        kobj: Option<&Arc<dyn KObject>>,
        target: &Arc<dyn KObject>,
        name: String,
    ) -> Result<(), SystemError> {
        return self.do_create_link(kobj, target, name, true);
    }

    /// 在sysfs中删除一个符号链接
    ///
    /// ## 参数
    ///
    /// - `kobj`: 要删除符号链接的kobject（符号链接所在目录）
    /// - `name`: 符号链接的名称
    ///
    ///
    /// 参考：https://code.dragonos.org.cn/xref/linux-6.1.9/fs/sysfs/symlink.c#143
    pub fn remove_link(&self, _kobj: &Arc<dyn KObject>, _name: String) {
        todo!("sysfs remove link")
    }

    fn do_create_link(
        &self,
        kobj: Option<&Arc<dyn KObject>>,
        target: &Arc<dyn KObject>,
        name: String,
        warn: bool,
    ) -> Result<(), SystemError> {
        let parent = if let Some(kobj) = kobj {
            kobj.inode()
        } else {
            Some(self.root_inode().clone())
        };

        // 没有parent，返回错误
        let parent = parent.ok_or(SystemError::EFAULT)?;
        return self.do_create_link_sd(&parent, target, name, warn);
    }

    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/fs/sysfs/symlink.c#20
    fn do_create_link_sd(
        &self,
        inode: &Arc<KernFSInode>,
        target: &Arc<dyn KObject>,
        name: String,
        warn: bool,
    ) -> Result<(), SystemError> {
        let target_inode = target.inode().ok_or(SystemError::ENOENT)?;

        let target_abs_path = "/sys".to_string() + &self.kernfs_path(&target_inode).to_owned();
        // let current_path = self.kernfs_path(inode);
        // debug!("sysfs: create link {} to {}", current_path, target_abs_path);

        let kn = inode.add_link(name.clone(), &target_inode, target_abs_path);
        if kn.is_ok() {
            return Ok(());
        }
        let err = kn.unwrap_err();
        if warn && err == SystemError::EEXIST {
            self.warn_duplicate(inode, &name);
        }
        return Err(err);
    }

    /// sysfs_create_link_sd - create symlink to a given object.
    ///
    /// ## 参数
    ///
    /// - `inode`: 目录inode，在这个目录下创建符号链接
    /// - `target`: object we're pointing to.
    /// - `name`: 符号链接的名称
    #[allow(dead_code)]
    pub(super) fn create_link_sd(
        &self,
        inode: &Arc<KernFSInode>,
        target: &Arc<dyn KObject>,
        name: String,
    ) -> Result<(), SystemError> {
        return self.do_create_link_sd(inode, target, name, true);
    }
}
