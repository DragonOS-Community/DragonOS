use alloc::{string::String, sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::filesystem::vfs::IndexNode;

/// 动态查找 trait，用于支持动态目录查找和列表
pub trait DynamicLookup: Send + Sync + core::fmt::Debug {
    /// 动态查找指定名称的条目
    /// 
    /// # 参数
    /// - `name`: 要查找的条目名称
    /// 
    /// # 返回值
    /// - `Ok(Some(inode))`: 找到了动态条目
    /// - `Ok(None)`: 没有找到动态条目，应该继续使用静态查找
    /// - `Err(error)`: 查找过程中发生错误
    fn dynamic_find(&self, name: &str) -> Result<Option<Arc<dyn IndexNode>>, SystemError>;

    /// 动态列出所有条目
    /// 
    /// # 返回值
    /// - `Ok(entries)`: 动态条目列表
    /// - `Err(error)`: 列表过程中发生错误
    fn dynamic_list(&self) -> Result<Vec<String>, SystemError>;

    /// 检查指定名称的条目是否有效（不创建条目）
    /// 
    /// # 参数
    /// - `name`: 要检查的条目名称
    /// 
    /// # 返回值
    /// - `true`: 条目有效
    /// - `false`: 条目无效
    fn is_valid_entry(&self, name: &str) -> bool;

    /// 创建临时条目（可选实现）
    /// 
    /// 这个方法允许动态查找提供者创建真正的临时条目，
    /// 这些条目不会被添加到父目录的 children 中。
    /// 
    /// # 参数
    /// - `name`: 要创建的条目名称
    /// 
    /// # 返回值
    /// - `Ok(Some(inode))`: 成功创建临时条目
    /// - `Ok(None)`: 不支持或不需要创建临时条目
    /// - `Err(error)`: 创建过程中发生错误
    fn create_temporary_entry(&self, _name: &str) -> Result<Option<Arc<dyn IndexNode>>, SystemError> {
        Ok(None) // 默认实现不创建临时条目
    }
}

