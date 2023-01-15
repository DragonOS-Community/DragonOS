use core::sync::atomic::AtomicUsize;
use core::sync::atomic::Ordering;

use super::InodeId;

/// @brief 原子地生成新的Inode号。
/// 请注意，所有的inode号都需要通过该函数来生成.全局的inode号，除了以下两个特殊的以外，都是唯一的
/// 特殊的两个inode号：
/// [0]: 对应'.'目录项
/// [1]: 对应'..'目录项
pub fn generate_inode_id() -> InodeId {
    static INO: AtomicUsize = AtomicUsize::new(1);
    return INO.fetch_add(1, Ordering::SeqCst);
}
