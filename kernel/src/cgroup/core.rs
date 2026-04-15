use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use hashbrown::{HashMap, HashSet};
use system_error::SystemError;

use crate::{
    libs::{rwlock::RwLock, spinlock::SpinLock},
    process::RawPid,
};

#[derive(Debug)]
pub struct CgroupNode {
    id: usize,
    name: String,
    parent: Option<Weak<CgroupNode>>,
    children: RwLock<HashMap<String, Arc<CgroupNode>>>,
    tasks: RwLock<HashSet<RawPid>>,
    //任务集合
    subtree_control: RwLock<HashSet<String>>,
    pids_max: RwLock<Option<usize>>,
    pids_events_max: AtomicU64,
    subtree_task_counter: AtomicUsize,
}

impl CgroupNode {
    fn new_root() -> Arc<Self> {
        Arc::new(Self {
            id: 1,
            name: String::new(),
            parent: None,
            children: RwLock::new(HashMap::new()),
            tasks: RwLock::new(HashSet::new()),
            subtree_control: RwLock::new(HashSet::new()),
            pids_max: RwLock::new(None),
            pids_events_max: AtomicU64::new(0),
            subtree_task_counter: AtomicUsize::new(0),
        })
    }

    fn new_child(id: usize, name: String, parent: &Arc<CgroupNode>) -> Arc<Self> {
        Arc::new(Self {
            id,
            name,
            parent: Some(Arc::downgrade(parent)),
            children: RwLock::new(HashMap::new()),
            tasks: RwLock::new(HashSet::new()),
            subtree_control: RwLock::new(HashSet::new()),
            pids_max: RwLock::new(None),
            pids_events_max: AtomicU64::new(0),
            subtree_task_counter: AtomicUsize::new(0),
        })
    }

    pub fn id(&self) -> usize {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn parent(&self) -> Option<Arc<CgroupNode>> {
        self.parent.as_ref().and_then(|p| p.upgrade())
    }

    pub fn add_task(&self, pid: RawPid) {
        self.tasks.write().insert(pid);
        let mut cur = self.parent();
        while let Some(node) = cur {
            node.subtree_task_counter.fetch_add(1, Ordering::Release);
            cur = node.parent();
        }
    }

    pub fn remove_task(&self, pid: RawPid) {
        self.tasks.write().remove(&pid);
        let mut cur = self.parent();
        while let Some(node) = cur {
            node.subtree_task_counter.fetch_sub(1, Ordering::Release);
            cur = node.parent();
        }
    }

    pub fn tasks(&self) -> Vec<RawPid> {
        self.tasks.read().iter().cloned().collect()
    }

    pub fn children_names(&self) -> Vec<String> {
        self.children.read().keys().cloned().collect()
    }

    pub fn children(&self) -> Vec<Arc<CgroupNode>> {
        self.children.read().values().cloned().collect()
    }

    pub fn child(&self, name: &str) -> Option<Arc<CgroupNode>> {
        self.children.read().get(name).cloned()
    }

    pub fn has_children(&self) -> bool {
        !self.children.read().is_empty()
    }

    pub fn has_tasks(&self) -> bool {
        !self.tasks.read().is_empty()
    }

    pub fn subtree_control(&self) -> Vec<String> {
        self.subtree_control.read().iter().cloned().collect()
    }

    pub fn set_subtree_control(&self, controllers: HashSet<String>) {
        *self.subtree_control.write() = controllers;
    }

    pub fn pids_max(&self) -> Option<usize> {
        *self.pids_max.read()
    }

    pub fn set_pids_max(&self, max: Option<usize>) {
        *self.pids_max.write() = max;
    }

    pub fn pids_events_max(&self) -> u64 {
        self.pids_events_max.load(Ordering::Relaxed)
    }

    pub fn inc_pids_events_max(&self) {
        self.pids_events_max.fetch_add(1, Ordering::Relaxed);
    }

    pub fn subtree_task_counter(&self) -> &AtomicUsize {
        &self.subtree_task_counter
    }

    pub fn subtree_task_count(&self) -> usize {
        self.tasks
            .read()
            .len()
            .saturating_add(self.subtree_task_counter.load(Ordering::Acquire))
    }

    pub fn is_ancestor_of(self: &Arc<Self>, other: &Arc<Self>) -> bool {
        if Arc::ptr_eq(self, other) {
            return true;
        }

        let mut cur = other.parent();
        while let Some(node) = cur {
            if Arc::ptr_eq(self, &node) {
                return true;
            }
            cur = node.parent();
        }

        false
    }
}

#[derive(Debug)]
pub struct CgroupRoot {
    root: Arc<CgroupNode>,
    next_id: AtomicUsize,
    all_nodes: SpinLock<HashMap<usize, Arc<CgroupNode>>>,
}

impl CgroupRoot {
    fn new() -> Arc<Self> {
        let root = CgroupNode::new_root();
        let mut all_nodes = HashMap::new();
        all_nodes.insert(root.id(), root.clone());

        Arc::new(Self {
            root,
            next_id: AtomicUsize::new(2),
            all_nodes: SpinLock::new(all_nodes),
        })
    }

    pub fn root(&self) -> Arc<CgroupNode> {
        self.root.clone()
    }

    #[allow(dead_code)]
    pub fn lookup_by_id(&self, id: usize) -> Option<Arc<CgroupNode>> {
        self.all_nodes.lock().get(&id).cloned()
    }

    pub fn create_child(
        &self,
        parent: &Arc<CgroupNode>,
        name: &str,
    ) -> Result<Arc<CgroupNode>, SystemError> {
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            return Err(SystemError::EINVAL);
        }
        //先找寻有无节点，避免重复创建
        {
            let children = parent.children.read();
            if let Some(existing) = children.get(name) {
                return Ok(existing.clone());
            }
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let child = CgroupNode::new_child(id, name.to_string(), parent);

        {
            let mut children = parent.children.write();
            if let Some(existing) = children.get(name) {
                return Ok(existing.clone());
            }
            children.insert(name.to_string(), child.clone());
        }

        self.all_nodes.lock().insert(id, child.clone());
        Ok(child)
    }

    pub fn remove_child(&self, parent: &Arc<CgroupNode>, name: &str) -> Result<(), SystemError> {
        let child = {
            let children = parent.children.read();
            children.get(name).cloned().ok_or(SystemError::ENOENT)?
        };
        //有孩子时返回busy错误
        if child.has_children() || child.has_tasks() {
            return Err(SystemError::EBUSY);
        }

        parent.children.write().remove(name);
        self.all_nodes.lock().remove(&child.id());
        Ok(())
    }

    #[allow(dead_code)]
    pub fn find_or_create_path(&self, path: &str) -> Result<Arc<CgroupNode>, SystemError> {
        let rel = normalize_cgroup_abs_path(path)?;
        let mut cur = self.root();

        if rel.is_empty() {
            return Ok(cur);
        }

        for comp in rel.split('/') {
            if comp.is_empty() {
                continue;
            }
            cur = self.create_child(&cur, comp)?;
        }

        Ok(cur)
    }

    #[allow(dead_code)]
    pub fn find_path(&self, path: &str) -> Result<Arc<CgroupNode>, SystemError> {
        let rel = normalize_cgroup_abs_path(path)?;
        let mut cur = self.root();

        if rel.is_empty() {
            return Ok(cur);
        }

        for comp in rel.split('/') {
            if comp.is_empty() {
                continue;
            }
            let next = cur
                .children
                .read()
                .get(comp)
                .cloned()
                .ok_or(SystemError::ENOENT)?;
            cur = next;
        }

        Ok(cur)
    }
}

#[derive(Debug, Clone)]
pub struct TaskCgroupRef {
    node: Arc<CgroupNode>,
}

impl TaskCgroupRef {
    pub fn new(node: Arc<CgroupNode>) -> Self {
        Self { node }
    }

    pub fn node(&self) -> Arc<CgroupNode> {
        self.node.clone()
    }
}

lazy_static! {
    static ref CGROUP_ROOT: Arc<CgroupRoot> = CgroupRoot::new();
    static ref CGROUP_ACCOUNTING_LOCK: SpinLock<()> = SpinLock::new(());
}

pub fn cgroup_root() -> &'static Arc<CgroupRoot> {
    &CGROUP_ROOT
}

pub fn cgroup_root_node() -> Arc<CgroupNode> {
    CGROUP_ROOT.root()
}

pub fn cgroup_accounting_lock() -> &'static SpinLock<()> {
    &CGROUP_ACCOUNTING_LOCK
}

pub fn cgroup_path_relative_to_node(node: &Arc<CgroupNode>, view_root: &Arc<CgroupNode>) -> String {
    if !view_root.is_ancestor_of(node) {
        return "/".to_string();
    }

    let node_path = cgroup_path_components(node);
    let root_path = cgroup_path_components(view_root);

    let down = &node_path[root_path.len()..];

    if down.is_empty() {
        return "/".to_string();
    }

    format!("/{}", down.join("/"))
}

fn cgroup_path_projected_from_view(node: &Arc<CgroupNode>, view_root: &Arc<CgroupNode>) -> String {
    let node_path = cgroup_path_components(node);
    let root_path = cgroup_path_components(view_root);
    let common = cgroup_common_ancestor(node, view_root);
    let common_depth = cgroup_path_components(&common).len();

    let up = root_path.len().saturating_sub(common_depth);
    let down = &node_path[common_depth..];

    if up == 0 && down.is_empty() {
        return "/".to_string();
    }

    let mut parts = Vec::with_capacity(up + down.len());
    for _ in 0..up {
        parts.push("..".to_string());
    }
    parts.extend(down.iter().cloned());

    format!("/{}", parts.join("/"))
}

pub fn cgroup_path_from_view(node: &Arc<CgroupNode>, view_root: &Arc<CgroupNode>) -> String {
    cgroup_path_projected_from_view(node, view_root)
}

pub fn cgroup_common_ancestor(left: &Arc<CgroupNode>, right: &Arc<CgroupNode>) -> Arc<CgroupNode> {
    let mut cur = Some(left.clone());
    while let Some(node) = cur {
        if node.is_ancestor_of(right) {
            return node;
        }
        cur = node.parent();
    }
    cgroup_root_node()
}
//一个已经作为管理节点的node不能同时作为迁移目的地承载普通节点
pub fn cgroup_migrate_vet_dst(dst: &Arc<CgroupNode>) -> Result<(), SystemError> {
    // v2 no-internal-process 约束：只要启用了 subtree_control 就禁止迁移进程
    if !dst.subtree_control().is_empty() {
        return Err(SystemError::EBUSY);
    }
    Ok(())
}
//fork前pids.max检查
pub fn cgroup_can_fork_in(node: &Arc<CgroupNode>, new_tasks: usize) -> Result<(), SystemError> {
    let mut cur = Some(node.clone());
    while let Some(cg) = cur {
        if let Some(max) = cg.pids_max() {
            let used = cg.subtree_task_count();
            if used.saturating_add(new_tasks) > max {
                cg.inc_pids_events_max();
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
        }
        cur = cg.parent();
    }
    Ok(())
}

pub fn cgroup_migrate_vet_dst_with_src(
    src: &Arc<CgroupNode>,
    dst: &Arc<CgroupNode>,
    moved_tasks: usize,
) -> Result<(), SystemError> {
    cgroup_migrate_vet_dst(dst)?;

    let mut cur = Some(dst.clone());
    while let Some(cg) = cur {
        if let Some(max) = cg.pids_max() {
            let used = cg.subtree_task_count();
            let delta = if cg.is_ancestor_of(src) {
                0
            } else {
                moved_tasks
            };
            if used.saturating_add(delta) > max {
                cg.inc_pids_events_max();
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
        }
        cur = cg.parent();
    }

    Ok(())
}

#[allow(dead_code)]
pub fn find_or_create_node_by_abs_path(path: &str) -> Result<Arc<CgroupNode>, SystemError> {
    cgroup_root().find_or_create_path(path)
}

#[allow(dead_code)]
pub fn find_node_by_abs_path(path: &str) -> Result<Arc<CgroupNode>, SystemError> {
    cgroup_root().find_path(path)
}

fn cgroup_path_components(node: &Arc<CgroupNode>) -> Vec<String> {
    let mut rev = Vec::new();
    let mut cur = Some(node.clone());

    while let Some(n) = cur {
        if !n.name().is_empty() {
            rev.push(n.name().to_string());
        }
        cur = n.parent();
    }

    rev.reverse();
    rev
}

fn normalize_cgroup_abs_path(path: &str) -> Result<String, SystemError> {
    // 支持两种形式：
    // 1) cgroup v2 路径："/foo/bar"
    // 2) 绝对挂载路径："/sys/fs/cgroup/foo/bar"
    let rel = if let Some(stripped) = path.strip_prefix("/sys/fs/cgroup") {
        stripped
    } else {
        path
    };

    if rel.is_empty() {
        return Ok(String::new());
    }

    if !rel.starts_with('/') {
        return Err(SystemError::EINVAL);
    }

    let mut out = Vec::new();
    //单调栈处理..和.
    for comp in rel.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp == ".." {
            if out.pop().is_none() {
                return Err(SystemError::EINVAL);
            }
            continue;
        }
        out.push(comp);
    }

    Ok(out.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgroup_path_from_view_same_node_is_root() {
        let root = CgroupRoot::new();
        let node = root.create_child(&root.root(), "same").unwrap();

        assert_eq!(cgroup_path_from_view(&node, &node), "/");
    }

    #[test]
    fn cgroup_path_from_view_descendant_stays_relative() {
        let root = CgroupRoot::new();
        let parent = root.create_child(&root.root(), "parent").unwrap();
        let child = root.create_child(&parent, "child").unwrap();

        assert_eq!(cgroup_path_from_view(&child, &parent), "/child");
    }

    #[test]
    fn cgroup_path_from_view_sibling_uses_parent_segments() {
        let root = CgroupRoot::new();
        let left = root.create_child(&root.root(), "left").unwrap();
        let right = root.create_child(&root.root(), "right").unwrap();

        assert_eq!(cgroup_path_from_view(&right, &left), "/../right");
    }
}
