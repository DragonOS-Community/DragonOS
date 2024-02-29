use alloc::rc::Weak;

use crate::{filesystem::vfs::{FileSystem, Metadata}, libs::spinlock::SpinLock};

#[derive(Debug)]
pub struct Ext2FsInfo {}

impl FileSystem for Ext2FsInfo {
    fn root_inode(&self) -> alloc::sync::Arc<dyn crate::filesystem::vfs::IndexNode> {
        todo!()
    }

    fn info(&self) -> crate::filesystem::vfs::FsInfo {
        todo!()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        todo!()
    }
}

#[derive(Debug)]
pub struct LockedExt2Inode(SpinLock<Ext2Inode>);

#[derive(Debug)]
pub struct Ext2Inode {

    /// 指向自身的弱引用
    self_ref: Weak<LockedExt2Inode>,

    /// 当前inode的元数据
    metadata: Metadata,



    /// TODO 一级指针
    direct_block:Weak<LockedExt2Inode>,
    /// TODO 二级指针
    indirect_block:Weak<LockedExt2Inode>,
    /// TODO 三级指针
    three_level_block:Weak<LockedExt2Inode>,

}
