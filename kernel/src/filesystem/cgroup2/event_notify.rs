//! Inode-identity notifications, independent of open file descriptions.

use alloc::sync::Arc;
use core::sync::atomic::Ordering;

use crate::{
    cgroup::{memory::MemoryAccount, CgroupNode},
    filesystem::{
        fsnotify::{fsnotify_targets, FsEvent},
        vfs::IndexNode,
    },
    mm::memory_notify::weak_registry::WeakRegistry,
};

use super::inode::Cgroup2Inode;

pub(super) static INODES: WeakRegistry<Cgroup2Inode> = WeakRegistry::new();

#[derive(Debug)]
pub(super) enum EventSource {
    Memory {
        account: Arc<MemoryAccount>,
        local: bool,
    },
    Population(Arc<CgroupNode>),
}

impl EventSource {
    pub(super) fn sequence(&self) -> u64 {
        match self {
            Self::Memory { account, local } => account.event_sequence(*local),
            Self::Population(node) => node.population_sequence(),
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Memory { local: false, .. } => "memory.events",
            Self::Memory { local: true, .. } => "memory.events.local",
            Self::Population(_) => "cgroup.events",
        }
    }
}

pub(crate) fn has_watchers() -> bool {
    INODES.has_registrations()
}

pub(crate) fn notify() {
    INODES.for_each(
        |inode| {
            let Some(source) = inode.event_source.as_ref() else {
                return;
            };
            let Some(fs) = inode.event_filesystem() else {
                return;
            };
            let Some(delivery) = inode.event_delivery() else {
                return;
            };
            let (sequence, child, parent) = delivery;
            let child: Arc<dyn IndexNode> = child;
            let parent: Option<Arc<dyn IndexNode>> =
                parent.map(|inode| inode as Arc<dyn IndexNode>);
            // Watch registration goes through MountFSInode, so bare dev_id=0
            // is not its object identity. This exact backend owns the same
            // canonical SB used by every bind/namespace projection.
            let Ok(child_target) = fs.event_target(&child) else {
                return;
            };
            let parent_target = parent
                .as_ref()
                .and_then(|parent| fs.event_target(parent).ok());
            fsnotify_targets(
                FsEvent::MODIFY,
                parent_target.as_ref().map(|parent| (parent, source.name())),
                Some(&child_target),
                0,
                false,
            );
            // A producer during delivery remains pending for the next pass.
            inode.event_seen.store(sequence, Ordering::Release);
        },
        |inode| inode.event_registration.load(Ordering::Acquire) != 0,
    );
}
