use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::cmp::Ordering;
use core::fmt::Debug;
use core::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

use crate::libs::spinlock::SpinLock;
use crate::process::{Cred, ProcessManager};
use system_error::SystemError;

use super::nsproxy::NsCommon;
use super::{NamespaceOps, NamespaceType};

/// UID/GID 映射区间
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UidGidExtent {
    /// 子命名空间中的起始 ID
    pub first: u32,
    /// 父命名空间中的起始 ID（写入后会被转换为 kernel-global ID）
    pub lower_first: u32,
    /// 映射数量
    pub count: u32,
}

/// 内联 extent 最大数量
pub const UID_GID_MAP_MAX_BASE_EXTENTS: usize = 5;
/// 最大 extent 数量（与 Linux 一致）
pub const UID_GID_MAP_MAX_EXTENTS: usize = 340;

/// 允许 setgroups 的标志位
pub const USERNS_SETGROUPS_ALLOWED: u32 = 1;

/// UID/GID 映射表
/// 采用小映射优化：≤5 个 extent 使用内联数组，否则堆分配
pub struct UidGidMap {
    /// 当前 extent 数量（使用原子操作保证并发可见性）
    pub nr_extents: AtomicU32,
    /// 内联存储（≤5 extents）
    pub extent: [UidGidExtent; UID_GID_MAP_MAX_BASE_EXTENTS],
    /// 堆分配的前向排序数组（按 first 排序，用于 map_id_down）
    pub forward: Option<Vec<UidGidExtent>>,
    /// 堆分配的反向排序数组（按 lower_first 排序，用于 map_id_up）
    pub reverse: Option<Vec<UidGidExtent>>,
}

impl Default for UidGidMap {
    fn default() -> Self {
        Self {
            nr_extents: AtomicU32::new(0),
            extent: [UidGidExtent {
                first: 0,
                lower_first: 0,
                count: 0,
            }; UID_GID_MAP_MAX_BASE_EXTENTS],
            forward: None,
            reverse: None,
        }
    }
}

impl UidGidMap {
    /// 创建 identity mapping（用于 init_user_ns）
    pub fn new_identity() -> Self {
        let mut map = Self::default();
        map.extent[0] = UidGidExtent {
            first: 0,
            lower_first: 0,
            count: u32::MAX,
        };
        map.nr_extents.store(1, AtomicOrdering::Release);
        map
    }

    /// 检查映射是否已写入（只写一次）
    pub fn is_written(&self) -> bool {
        self.nr_extents.load(AtomicOrdering::Acquire) != 0
    }

    /// 获取 extent 数量
    pub fn get_nr_extents(&self) -> u32 {
        self.nr_extents.load(AtomicOrdering::Acquire)
    }
}

/// 将 id 从 child namespace 映射到 parent namespace（map_id_down）
/// 在 map 的 extent 中查找，child id 匹配 extent.first
pub fn map_id_down(map: &UidGidMap, id: u32) -> Option<u32> {
    let nr = map.get_nr_extents() as usize;
    if nr == 0 {
        return None;
    }

    let extents: &[UidGidExtent] = if nr <= UID_GID_MAP_MAX_BASE_EXTENTS {
        &map.extent[..nr]
    } else {
        map.forward.as_deref().unwrap_or(&[])
    };

    // 线性扫描或二分查找
    let idx = if nr <= UID_GID_MAP_MAX_BASE_EXTENTS {
        extents
            .iter()
            .position(|e| id >= e.first && id < e.first.saturating_add(e.count))?
    } else {
        // 二分查找（forward 按 first 排序）
        match extents.binary_search_by(|e| e.first.cmp(&id)) {
            Ok(i) => i,
            Err(i) => {
                if i == 0 {
                    return None;
                }
                // 检查前一个 extent 是否包含 id
                let e = &extents[i - 1];
                if id >= e.first && id < e.first.saturating_add(e.count) {
                    i - 1
                } else {
                    return None;
                }
            }
        }
    };

    let e = &extents[idx];
    Some((id - e.first) + e.lower_first)
}

/// 将 id 从 parent namespace 映射到 child namespace（map_id_up）
/// 在 map 的 extent 中查找，parent id 匹配 extent.lower_first
pub fn map_id_up(map: &UidGidMap, id: u32) -> Option<u32> {
    let nr = map.get_nr_extents() as usize;
    if nr == 0 {
        return None;
    }

    let extents: &[UidGidExtent] = if nr <= UID_GID_MAP_MAX_BASE_EXTENTS {
        &map.extent[..nr]
    } else {
        map.reverse.as_deref().unwrap_or(&[])
    };

    let idx = if nr <= UID_GID_MAP_MAX_BASE_EXTENTS {
        extents
            .iter()
            .position(|e| id >= e.lower_first && id < e.lower_first.saturating_add(e.count))?
    } else {
        match extents.binary_search_by(|e| e.lower_first.cmp(&id)) {
            Ok(i) => i,
            Err(i) => {
                if i == 0 {
                    return None;
                }
                let e = &extents[i - 1];
                if id >= e.lower_first && id < e.lower_first.saturating_add(e.count) {
                    i - 1
                } else {
                    return None;
                }
            }
        }
    };

    let e = &extents[idx];
    Some((id - e.lower_first) + e.first)
}

/// 范围 down 映射：验证 [id, id+count) 都能被映射
pub fn map_id_range_down(map: &UidGidMap, id: u32, count: u32) -> Option<u32> {
    if count == 0 {
        return Some(id);
    }
    let end = id.saturating_add(count - 1);
    let mapped_start = map_id_down(map, id)?;
    let mapped_end = map_id_down(map, end)?;
    // 验证映射是连续的
    if mapped_end != mapped_start.saturating_add(count - 1) {
        return None;
    }
    Some(mapped_start)
}

lazy_static! {
    pub static ref INIT_USER_NAMESPACE: Arc<UserNamespace> = UserNamespace::new_root();
}

pub struct UserNamespace {
    pub parent: Option<Weak<UserNamespace>>,
    nscommon: NsCommon,
    self_ref: Weak<UserNamespace>,
    pub inner: SpinLock<InnerUserNamespace>,
}

pub struct InnerUserNamespace {
    pub children: Vec<Arc<UserNamespace>>,
    /// UID 映射表
    pub uid_map: UidGidMap,
    /// GID 映射表
    pub gid_map: UidGidMap,
    /// Project ID 映射表（预留）
    pub projid_map: UidGidMap,
    /// 所有者 UID（在父命名空间中的 kernel-global ID，用 usize 存储 Kuid）
    pub owner: usize,
    /// 所有者 GID
    pub group: usize,
    /// 标志位 (USERNS_SETGROUPS_ALLOWED)
    pub flags: u32,
    /// 创建时父进程是否有 CAP_SETFCAP
    pub parent_could_setfcap: bool,
}

impl NamespaceOps for UserNamespace {
    fn ns_common(&self) -> &NsCommon {
        &self.nscommon
    }
}

impl UserNamespace {
    /// 创建 root user namespace
    fn new_root() -> Arc<Self> {
        Arc::new_cyclic(|self_ref| Self {
            self_ref: self_ref.clone(),
            nscommon: NsCommon::new(0, NamespaceType::User),
            parent: None,
            inner: SpinLock::new(InnerUserNamespace {
                children: Vec::new(),
                uid_map: UidGidMap::new_identity(),
                gid_map: UidGidMap::new_identity(),
                projid_map: UidGidMap::default(),
                owner: 0,
                group: 0,
                flags: USERNS_SETGROUPS_ALLOWED,
                parent_could_setfcap: true,
            }),
        })
    }

    /// 获取层级
    pub fn level(&self) -> u32 {
        self.nscommon.level
    }

    /// 获取父命名空间
    pub fn parent_ns(&self) -> Option<Arc<UserNamespace>> {
        self.parent.as_ref().and_then(|p| p.upgrade())
    }

    /// 检查当前用户命名空间是否是另一个用户命名空间的祖先
    pub fn is_ancestor_of(&self, other: &Arc<Self>) -> bool {
        let mut current = other.clone();
        let self_level = self.level();
        loop {
            let current_level = current.level();
            match current_level.cmp(&self_level) {
                Ordering::Greater => {
                    if let Some(parent) = current.parent.as_ref().and_then(|p| p.upgrade()) {
                        current = parent;
                        continue;
                    } else {
                        return false;
                    }
                }
                Ordering::Equal => return Arc::ptr_eq(&self.self_ref.upgrade().unwrap(), &current),
                Ordering::Less => return false,
            }
        }
    }

    /// 创建新的 user namespace（对应 Linux create_user_ns）
    ///
    /// 调用者提供当前进程的 cred，函数会基于 cred 的 user_ns 作为父 namespace
    /// 创建新的 user namespace，并返回新 namespace 的 Arc。
    ///
    /// 注意：此函数**不**修改 cred，调用者需要自行调用 set_cred_user_ns。
    pub fn create_user_ns(cred: &Cred) -> Result<Arc<Self>, SystemError> {
        let parent_ns = cred.user_ns.clone();

        // 1. 嵌套深度检查
        if parent_ns.level() >= 32 {
            return Err(SystemError::ENOSPC);
        }

        // 2. chroot 检查（简化版：检查当前 fs.root 是否与 init 不同）
        // TODO: 实现更严格的 chroot 检查

        // 3. 创建者的 euid/egid 在父 ns 中必须有有效映射
        // 对于 init_user_ns，这总是成立的（identity mapping）
        // 对于子 ns，需要验证映射存在

        // 4. 创建新的 UserNamespace
        let new_ns = Arc::new_cyclic(|self_ref| {
            let ns = Self {
                self_ref: self_ref.clone(),
                nscommon: NsCommon::new(parent_ns.level() + 1, NamespaceType::User),
                parent: Some(Arc::downgrade(&parent_ns)),
                inner: SpinLock::new(InnerUserNamespace {
                    children: Vec::new(),
                    uid_map: UidGidMap::default(),
                    gid_map: UidGidMap::default(),
                    projid_map: UidGidMap::default(),
                    owner: cred.euid.data(),
                    group: cred.egid.data(),
                    flags: USERNS_SETGROUPS_ALLOWED,
                    parent_could_setfcap: cred
                        .cap_effective
                        .contains(crate::process::cred::CAPFlags::CAP_SETFCAP),
                }),
            };
            ns
        });

        // 5. 将新 ns 添加到父 ns 的 children 列表
        {
            let mut parent_inner = parent_ns.inner.lock();
            parent_inner.children.push(new_ns.clone());
        }

        Ok(new_ns)
    }
}

/// 检查 user namespace 中是否允许 setgroups
///
/// 需要同时满足：
/// 1. gid_map 已写入（有有效的 GID 映射）
/// 2. setgroups 未被拒绝（USERNS_SETGROUPS_ALLOWED 标志）
pub fn userns_may_setgroups(ns: &Arc<UserNamespace>) -> bool {
    let inner = ns.inner.lock();
    inner.gid_map.is_written() && (inner.flags & USERNS_SETGROUPS_ALLOWED) != 0
}

impl Debug for UserNamespace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UserNamespace")
            .field("level", &self.level())
            .finish()
    }
}

impl ProcessManager {
    /// 获取当前进程的 user_ns
    pub fn current_user_ns() -> Arc<UserNamespace> {
        if Self::initialized() {
            ProcessManager::current_pcb().cred().user_ns.clone()
        } else {
            INIT_USER_NAMESPACE.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn make_extent(first: u32, lower_first: u32, count: u32) -> UidGidExtent {
        UidGidExtent {
            first,
            lower_first,
            count,
        }
    }

    fn make_map_inline(extents: &[UidGidExtent]) -> UidGidMap {
        let mut map = UidGidMap::default();
        for (i, e) in extents.iter().enumerate() {
            map.extent[i] = *e;
        }
        map.nr_extents
            .store(extents.len() as u32, core::sync::atomic::Ordering::Release);
        map
    }

    fn make_map_heap(extents: &[UidGidExtent]) -> UidGidMap {
        let mut map = UidGidMap::default();
        let mut forward = extents.to_vec();
        forward.sort_by_key(|e| e.first);
        let mut reverse = extents.to_vec();
        reverse.sort_by_key(|e| e.lower_first);
        map.forward = Some(forward);
        map.reverse = Some(reverse);
        map.nr_extents
            .store(extents.len() as u32, core::sync::atomic::Ordering::Release);
        map
    }

    // ── map_id_down ──

    #[test]
    fn test_map_id_down_empty() {
        let map = UidGidMap::default();
        assert_eq!(map_id_down(&map, 0), None);
        assert_eq!(map_id_down(&map, 1000), None);
    }

    #[test]
    fn test_map_id_down_single_extent() {
        let map = make_map_inline(&[make_extent(0, 1000, 10)]);
        assert_eq!(map_id_down(&map, 0), Some(1000));
        assert_eq!(map_id_down(&map, 5), Some(1005));
        assert_eq!(map_id_down(&map, 9), Some(1009));
        assert_eq!(map_id_down(&map, 10), None);
    }

    #[test]
    fn test_map_id_down_offset_extent() {
        let map = make_map_inline(&[make_extent(100, 200, 50)]);
        assert_eq!(map_id_down(&map, 99), None);
        assert_eq!(map_id_down(&map, 100), Some(200));
        assert_eq!(map_id_down(&map, 149), Some(349));
        assert_eq!(map_id_down(&map, 150), None);
    }

    #[test]
    fn test_map_id_down_multiple_inline() {
        let map = make_map_inline(&[make_extent(0, 100, 10), make_extent(1000, 5000, 20)]);
        assert_eq!(map_id_down(&map, 5), Some(105));
        assert_eq!(map_id_down(&map, 1005), Some(5005));
        assert_eq!(map_id_down(&map, 500), None);
    }

    #[test]
    fn test_map_id_down_identity() {
        let map = UidGidMap::new_identity();
        assert_eq!(map_id_down(&map, 0), Some(0));
        assert_eq!(map_id_down(&map, 1000), Some(1000));
        assert_eq!(map_id_down(&map, u32::MAX - 1), Some(u32::MAX - 1));
    }

    #[test]
    fn test_map_id_down_heap_binary_search() {
        let extents: Vec<UidGidExtent> = (0..10)
            .map(|i| make_extent(i * 100, i * 1000, 50))
            .collect();
        let map = make_map_heap(&extents);
        assert_eq!(map_id_down(&map, 0), Some(0));
        assert_eq!(map_id_down(&map, 250), Some(2500));
        assert_eq!(map_id_down(&map, 949), Some(9400));
        assert_eq!(map_id_down(&map, 950), None);
        assert_eq!(map_id_down(&map, 5000), None);
    }

    // ── map_id_up ──

    #[test]
    fn test_map_id_up_empty() {
        let map = UidGidMap::default();
        assert_eq!(map_id_up(&map, 0), None);
    }

    #[test]
    fn test_map_id_up_single_extent() {
        let map = make_map_inline(&[make_extent(100, 500, 10)]);
        assert_eq!(map_id_up(&map, 500), Some(100));
        assert_eq!(map_id_up(&map, 509), Some(109));
        assert_eq!(map_id_up(&map, 499), None);
        assert_eq!(map_id_up(&map, 510), None);
    }

    #[test]
    fn test_map_id_up_identity() {
        let map = UidGidMap::new_identity();
        assert_eq!(map_id_up(&map, 0), Some(0));
        assert_eq!(map_id_up(&map, u32::MAX - 1), Some(u32::MAX - 1));
    }

    #[test]
    fn test_map_id_up_heap_binary_search() {
        let extents: Vec<UidGidExtent> = (0..10)
            .map(|i| make_extent(i * 100, i * 1000, 50))
            .collect();
        let map = make_map_heap(&extents);
        assert_eq!(map_id_up(&map, 0), Some(0));
        assert_eq!(map_id_up(&map, 2500), Some(250));
        assert_eq!(map_id_up(&map, 9400), Some(940));
        assert_eq!(map_id_up(&map, 9450), None);
    }

    // ── map_id_range_down ──

    #[test]
    fn test_map_id_range_down_zero_count() {
        let map = UidGidMap::default();
        assert_eq!(map_id_range_down(&map, 42, 0), Some(42));
    }

    #[test]
    fn test_map_id_range_down_within_extent() {
        let map = make_map_inline(&[make_extent(0, 100, 50)]);
        assert_eq!(map_id_range_down(&map, 0, 10), Some(100));
        assert_eq!(map_id_range_down(&map, 5, 20), Some(105));
    }

    #[test]
    fn test_map_id_range_down_cross_extent_fails() {
        let map = make_map_inline(&[make_extent(0, 100, 10), make_extent(100, 200, 10)]);
        // 跨越两个不连续的 extent，映射不连续
        assert_eq!(map_id_range_down(&map, 0, 20), None);
    }

    #[test]
    fn test_map_id_range_down_out_of_range() {
        let map = make_map_inline(&[make_extent(0, 100, 10)]);
        assert_eq!(map_id_range_down(&map, 0, 11), None);
        assert_eq!(map_id_range_down(&map, 10, 1), None);
    }

    // ── UidGidMap constructors ──

    #[test]
    fn test_default_map_not_written() {
        let map = UidGidMap::default();
        assert!(!map.is_written());
        assert_eq!(map.get_nr_extents(), 0);
    }

    #[test]
    fn test_identity_map_written() {
        let map = UidGidMap::new_identity();
        assert!(map.is_written());
        assert_eq!(map.get_nr_extents(), 1);
    }

    #[test]
    fn test_roundtrip_inline() {
        let map = make_map_inline(&[make_extent(0, 1000, 100)]);
        for id in 0..100u32 {
            let down = map_id_down(&map, id).unwrap();
            assert_eq!(down, id + 1000);
            let up = map_id_up(&map, down).unwrap();
            assert_eq!(up, id);
        }
    }

    #[test]
    fn test_roundtrip_heap() {
        let extents: Vec<UidGidExtent> = (0..8)
            .map(|i| make_extent(i * 100, i * 1000 + 500, 50))
            .collect();
        let map = make_map_heap(&extents);
        for i in 0..8u32 {
            for offset in [0, 25, 49] {
                let child_id = i * 100 + offset;
                let parent_id = map_id_down(&map, child_id).unwrap();
                assert_eq!(parent_id, i * 1000 + 500 + offset);
                let back = map_id_up(&map, parent_id).unwrap();
                assert_eq!(back, child_id);
            }
        }
    }
}
