#![allow(dead_code, unused_variables, unused_imports)]
pub mod mem_cgroup;

use alloc::{collections::LinkedList, rc::Weak, sync::Arc, vec::Vec};

use alloc::boxed::Box;

use crate::filesystem::vfs::IndexNode;

pub struct Cgroup {
    css: Weak<CgroupSubsysState>,
    /// 当前所在的深度
    level: u32,
    /// 支持的最大深度
    max_depth: u32,
    /// 可见后代数量
    nr_descendants: u32,
    /// 正在死亡后代数量
    nr_dying_descendants: u32,
    /// 允许的最大后代数量
    max_descendants: u32,
    /// css_set的数量
    nr_populated_csets: u32,
    /// 子group中有任务的记数
    nr_populated_domain_children: u32,
    /// 线程子group中有任务的记数
    nr_populated_threaded_children: u32,
    /// 活跃线程子cgroup数量
    nr_threaded_children: u32,
    /// 关联cgroup的inode
    kernfs_node: Box<dyn IndexNode>,
}

/// 控制资源的统计信息
pub struct CgroupSubsysState {
    cgroup: Arc<Cgroup>,
    /// 兄弟节点
    sibling: LinkedList<Arc<Cgroup>>,
    /// 孩子节点
    children: LinkedList<Arc<Cgroup>>,
}

pub struct CgroupSubsys {}

/// cgroup_sub_state 的集合
pub struct CssSet {
    subsys: Vec<Arc<CgroupSubsysState>>,
}
