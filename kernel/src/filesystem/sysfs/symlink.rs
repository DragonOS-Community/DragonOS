use alloc::{string::String, sync::Arc};

use crate::{driver::base::kobject::KObject, syscall::SystemError};

use super::SysFS;

impl SysFS {
    /// 在sysfs中创建一个符号链接
    ///
    /// ## 参数
    ///
    /// - `kobj`: 要创建符号链接的kobject
    /// - `target`: 符号链接的目标（在目标目录下创建）
    /// - `name`: 符号链接的名称
    /// 
    /// 参考：https://opengrok.ringotek.cn/xref/linux-6.1.9/fs/sysfs/symlink.c#89
    pub fn create_link(
        &self,
        kobj: &Arc<dyn KObject>,
        target: &Arc<dyn KObject>,
        name: String,
    ) -> Result<(), SystemError> {
        todo!("sysfs create link")
    }

    /// 在sysfs中删除一个符号链接
    ///
    /// ## 参数
    ///
    /// - `kobj`: 要删除符号链接的kobject（符号链接所在目录）
    /// - `name`: 符号链接的名称
    /// 
    /// 
    /// 参考：https://opengrok.ringotek.cn/xref/linux-6.1.9/fs/sysfs/symlink.c#143
    pub fn remove_link(&self, kobj: &Arc<dyn KObject>, name: String) -> Result<(), SystemError> {
        todo!("sysfs remove link")
    }
}
