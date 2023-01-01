#![allow(dead_code)]

use alloc::{string::String, collections::LinkedList};

/// vfs容许的最大的路径名称长度
pub const MAX_PATHLEN: u32 = 1024;

/**
 * 磁盘分区表类型
 */
#[repr(u8)]
#[derive(Debug)]
pub enum DiskPartitionTableType {
    MBR = 0,
    GPT = 1,
}

/**
 * inode的标志位（这里的数字表示第几位）
 */
#[repr(u8)]
#[derive(Debug)]
pub enum InodeFlags {
    /// 当前inode表示一个文件
    File = 0,
    /// 当前inode表示一个文件夹
    Dir = 1,
    /// 当前inode表示一个设备
    Device = 2,
    /// 当前inode事实上已经被删除，但是目录项仍处于open状态
    Dead = 3,
}

/**
 * dentry的标志位
 */
#[repr(u8)]
#[derive(Debug)]
pub enum DentryFlags {
    /// 当前dentry是一个挂载点
    Mounted = 0,
    /// 当前dentry不可被挂载
    CannotMount = 1,
}

#[derive(Debug)]
pub struct DirEntry<'a>{
    name: String,
    d_flags: u32,
    child_node_list:LinkedList<DirEntry<'a>>,
    lockref: LockRef,
    parent: &'a mut DirEntry<'a>,
    dir_ops: &'a DirEntryOps<'a>,
}

#[derive(Debug)]
pub struct DirEntryOps<'a>{
    compare: Option<fn(parent_dentry:&'a mut DirEntry, source_filename: &'a str, dest_filename: &'a str)->i64>,
    hash: Option<fn(dentry:&'a mut DirEntry)->i64>,
    release: Option<fn(dentry:&'a mut DirEntry)->i64>,
    iput: Option<fn(dentry:&'a mut DirEntry, inode:&'a mut IndexNode)->i64>
}

#[derive(Debug)]
pub struct IndexNode{
    
}
