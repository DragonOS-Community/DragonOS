use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use hashbrown::HashMap;

use crate::libs::spinlock::SpinLock;
use core::sync::atomic::{AtomicU32, Ordering};
use ida::IdAllocator;
use system_error::SystemError;

use super::inode::{InotifyInode, QueuedEvent};
use super::uapi::{InotifyCookie, InotifyMask, WatchDescriptor};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InodeKey {
    pub dev_id: usize,
    pub inode_id: usize,
}

impl InodeKey {
    pub fn new(dev_id: usize, inode_id: usize) -> Self {
        Self { dev_id, inode_id }
    }
}

#[derive(Debug)]
struct Watch {
    inode_key: InodeKey,
    wd: WatchDescriptor,
    mask: InotifyMask,
}

#[derive(Debug)]
struct InstanceInner {
    inode: Weak<InotifyInode>,
    watches: HashMap<WatchDescriptor, Watch>,
    wd_alloc: IdAllocator,
}

impl InstanceInner {
    fn new(inode: &Arc<InotifyInode>) -> Self {
        Self {
            inode: Arc::downgrade(inode),
            watches: HashMap::new(),
            wd_alloc: IdAllocator::new(1, i32::MAX as usize).unwrap(),
        }
    }
}

#[derive(Debug, Clone)]
struct WatchRef {
    instance_id: u32,
    wd: WatchDescriptor,
}

#[derive(Debug)]
struct Registry {
    instances: HashMap<u32, InstanceInner>,
    by_inode: HashMap<InodeKey, Vec<WatchRef>>,
}

impl Registry {
    fn new() -> Self {
        Self {
            instances: HashMap::new(),
            by_inode: HashMap::new(),
        }
    }

    fn ensure_instance(&mut self, inode: &Arc<InotifyInode>) {
        let instance_id = inode.instance_id();
        self.instances
            .entry(instance_id)
            .or_insert_with(|| InstanceInner::new(inode));
    }

    pub fn add_watch(
        &mut self,
        inode: &Arc<InotifyInode>,
        watch_key: InodeKey,
        mut mask: InotifyMask,
    ) -> Result<WatchDescriptor, SystemError> {
        self.ensure_instance(inode);

        let instance_id = inode.instance_id();
        let inner = self.instances.get_mut(&instance_id).unwrap();

        // Check existing watch for the same inode.
        if let Some((wd, existing_mask)) = inner
            .watches
            .iter()
            .find(|(_wd, w)| w.inode_key == watch_key)
            .map(|(wd, w)| (*wd, w.mask))
        {
            if mask.contains(InotifyMask::IN_MASK_ADD) {
                mask = existing_mask | (mask & !InotifyMask::IN_MASK_ADD);
            }
            inner.watches.insert(
                wd,
                Watch {
                    inode_key: watch_key,
                    wd,
                    mask,
                },
            );
            return Ok(wd);
        }

        let wd_raw = inner.wd_alloc.alloc().ok_or(SystemError::ENOMEM)? as i32;
        let wd = WatchDescriptor(wd_raw);

        inner.watches.insert(
            wd,
            Watch {
                inode_key: watch_key,
                wd,
                mask,
            },
        );

        self.by_inode
            .entry(watch_key)
            .or_default()
            .push(WatchRef { instance_id, wd });

        Ok(wd)
    }

    pub fn rm_watch(
        &mut self,
        inode: &Arc<InotifyInode>,
        wd: WatchDescriptor,
    ) -> Result<(), SystemError> {
        let instance_id = inode.instance_id();
        let (watch, inode_weak) = {
            let inner = self
                .instances
                .get_mut(&instance_id)
                .ok_or(SystemError::EINVAL)?;
            let watch = inner.watches.remove(&wd).ok_or(SystemError::EINVAL)?;
            inner.wd_alloc.free(wd.0 as usize);
            (watch, inner.inode.clone())
        };

        if let Some(list) = self.by_inode.get_mut(&watch.inode_key) {
            list.retain(|r| r.instance_id != instance_id || r.wd != wd);
            if list.is_empty() {
                self.by_inode.remove(&watch.inode_key);
            }
        }

        if let Some(inode_arc) = inode_weak.upgrade() {
            inode_arc.enqueue_event(QueuedEvent {
                wd,
                mask: InotifyMask::IN_IGNORED,
                cookie: InotifyCookie(0),
                name: None,
            })?;
        }

        Ok(())
    }

    pub fn find_targets(
        &self,
        inode_key: InodeKey,
        relevant_mask: InotifyMask,
    ) -> Vec<(Weak<InotifyInode>, WatchDescriptor, InotifyMask)> {
        let mut to_fire = Vec::new();
        let refs = match self.by_inode.get(&inode_key) {
            Some(v) => v,
            None => return to_fire,
        };
        for r in refs {
            if let Some(inner) = self.instances.get(&r.instance_id) {
                if let Some(w) = inner.watches.get(&r.wd) {
                    let wmask = w.mask;
                    if !relevant_mask.is_empty() && (wmask & relevant_mask).is_empty() {
                        continue;
                    }
                    to_fire.push((inner.inode.clone(), r.wd, wmask));
                }
            }
        }
        to_fire
    }
}

lazy_static::lazy_static! {
    static ref REGISTRY: SpinLock<Registry> = SpinLock::new(Registry::new());
}
static MOVE_COOKIE: AtomicU32 = AtomicU32::new(1);

pub fn register_instance(instance_id: u32) {
    // placeholder: instances are inserted on first `bind_inode`.
    let _ = instance_id;
}

pub fn unregister_instance(instance_id: u32) {
    let mut reg = REGISTRY.lock();
    if let Some(inner) = reg.instances.remove(&instance_id) {
        // remove all reverse mappings
        for (_wd, watch) in inner.watches.iter() {
            if let Some(list) = reg.by_inode.get_mut(&watch.inode_key) {
                list.retain(|r| r.instance_id != instance_id || r.wd != watch.wd);
                if list.is_empty() {
                    reg.by_inode.remove(&watch.inode_key);
                }
            }
        }
    }
}

pub fn add_watch(
    inode: &Arc<InotifyInode>,
    watch_key: InodeKey,
    mask: InotifyMask,
) -> Result<WatchDescriptor, SystemError> {
    REGISTRY.lock().add_watch(inode, watch_key, mask)
}

pub fn rm_watch(inode: &Arc<InotifyInode>, wd: WatchDescriptor) -> Result<(), SystemError> {
    REGISTRY.lock().rm_watch(inode, wd)
}

pub fn next_cookie() -> InotifyCookie {
    InotifyCookie(MOVE_COOKIE.fetch_add(1, Ordering::Relaxed))
}

pub fn report(inode_key: InodeKey, mask: InotifyMask) {
    report_internal(inode_key, mask, InotifyCookie(0), None);
}

#[allow(dead_code)]
pub fn report_inode(inode_key: InodeKey, mask: InotifyMask, cookie: InotifyCookie) {
    report_internal(inode_key, mask, cookie, None);
}

pub fn report_dir_entry(
    parent: InodeKey,
    mask: InotifyMask,
    cookie: InotifyCookie,
    name: &str,
    is_dir: bool,
) {
    let mut name_bytes = alloc::vec::Vec::with_capacity(name.len() + 1);
    name_bytes.extend_from_slice(name.as_bytes());
    name_bytes.push(0);

    let mask = if is_dir {
        mask | InotifyMask::IN_ISDIR
    } else {
        mask
    };
    report_internal(parent, mask, cookie, Some(name_bytes));
}

/// 当某个 inode 的“最后一个链接”被删除时，Linux 会给该 inode 上的 watch
/// 投递 `IN_DELETE_SELF`（目录为 `IN_DELETE_SELF|IN_ISDIR`），并紧接着投递 `IN_IGNORED`。
///
/// DragonOS 当前在 VFS 层（unlink/rmdir）调用该函数，确保 watch 被及时清理，避免
/// registry 中残留指向已删除对象的 watch。
pub fn report_delete_self_and_purge(inode_key: InodeKey, is_dir: bool) {
    // gvisor 测例期望：
    // - 被删除的普通文件（最后一个链接消失）先投递 IN_ATTRIB，再投递 IN_DELETE_SELF/IN_IGNORED。
    // - rmdir 目录目标仅投递 IN_DELETE_SELF/IN_IGNORED（且不包含 IN_ISDIR）。
    // 因此这里按 is_dir 做最小差异化实现。
    let self_mask = InotifyMask::IN_DELETE_SELF;

    // 先快照当前 inode 的所有 (instance, wd)，避免持锁执行 enqueue/rm_watch。
    let targets = {
        let reg = REGISTRY.lock();
        // Get all watches on this inode
        reg.find_targets(inode_key, InotifyMask::empty())
    };

    for (weak_inode, wd, mask) in targets {
        if let Some(inode) = weak_inode.upgrade() {
            if !is_dir && mask.contains(InotifyMask::IN_ATTRIB) {
                let _ = inode.enqueue_event(QueuedEvent {
                    wd,
                    mask: InotifyMask::IN_ATTRIB,
                    cookie: InotifyCookie(0),
                    name: None,
                });
            }
            if mask.contains(self_mask) {
                let _ = inode.enqueue_event(QueuedEvent {
                    wd,
                    mask: self_mask,
                    cookie: InotifyCookie(0),
                    name: None,
                });
            }
            // rm_watch 会投递 IN_IGNORED 并清理 registry。
            let _ = rm_watch(&inode, wd);
        }
    }
}

fn report_internal(
    inode_key: InodeKey,
    mask: InotifyMask,
    cookie: InotifyCookie,
    name: Option<Vec<u8>>,
) {
    // 注意：IN_ISDIR/IN_UNMOUNT/IN_Q_OVERFLOW/IN_IGNORED 等不是用户用于过滤的事件位。
    // 过滤逻辑仅使用“事件位”本身。
    let relevant = mask & InotifyMask::IN_ALL_EVENTS;
    let to_fire = REGISTRY.lock().find_targets(inode_key, relevant);

    for (weak_inode, wd, wmask) in to_fire {
        if let Some(inode) = weak_inode.upgrade() {
            let _ = inode.enqueue_event(QueuedEvent {
                wd,
                mask,
                cookie,
                name: name.clone(),
            });

            if wmask.contains(InotifyMask::IN_ONESHOT) {
                let _ = rm_watch(&inode, wd);
            }
        }
    }
}
