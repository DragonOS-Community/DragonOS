use super::*;

// ──────────────────────────── XOL 区 ────────────────────────────

/// 每个 slot 的宽度（= `UPROBE_INSN_COPY_SIZE` = 16 字节）。
const XOL_SLOT_SIZE: usize = UPROBE_INSN_COPY_SIZE;

/// 每页 slot 数量（4096 / 16 = 256）。
const XOL_SLOTS_PER_PAGE: usize = MMArch::PAGE_SIZE / XOL_SLOT_SIZE;

/// slot 位图需要的 u64 字数（256 bits → 4 words）。
const XOL_BITMAP_WORDS: usize = XOL_SLOTS_PER_PAGE.div_ceil(64);

/// Per-mm XOL（eXecute Out of Line）区。
///
/// 在用户地址空间映射一个可读可执行页，分成 16 字节对齐的 slot。每个 uprobe 分配一个 slot，
/// 命中时 batch3 在 slot 中写入原指令副本（RIP-relative 重定位后），rip 指向 slot 执行。
///
/// XOL 页在**注册时**（进程上下文、开中断）创建，**不能**在命中路径（关中断）创建。
pub struct XolArea {
    /// XOL 页在用户空间的基地址。
    page_base: VirtAddr,
    /// XOL 页的物理地址（供 batch3 在关中断路径下通过 `phys_2_virt` 直接写 slot 内容，
    /// 无需 mapper / RwSem）。
    page_paddr: PhysAddr,
    /// 保证 XOL 物理页覆盖整个租约生命周期；不能只保存裸物理地址。
    _page: Arc<Page>,
    /// 区域代次，用于阻止旧租约释放新区域的同号 slot。
    generation: u64,
    /// slot 分配位图（bit=1 表示已占用）。
    slot_bitmap: SpinLock<[u64; XOL_BITMAP_WORDS]>,
}

impl XolArea {
    pub(super) fn new(page_base: VirtAddr, page_paddr: PhysAddr, page: Arc<Page>) -> Arc<Self> {
        Arc::new(Self {
            page_base,
            page_paddr,
            _page: page,
            generation: NEXT_XOL_GENERATION.fetch_add(1, Ordering::Relaxed),
            slot_bitmap: SpinLock::new([0u64; XOL_BITMAP_WORDS]),
        })
    }

    pub(super) fn alloc_slot(self: &Arc<Self>) -> Option<XolSlotLease> {
        let mut bitmap = self.slot_bitmap.lock_irqsave();
        for (word_idx, word) in bitmap.iter_mut().enumerate() {
            if *word != u64::MAX {
                let bit = (!*word).trailing_zeros() as usize;
                let slot = word_idx * 64 + bit;
                if slot >= XOL_SLOTS_PER_PAGE {
                    break;
                }
                *word |= 1u64 << bit;
                return Some(XolSlotLease {
                    area: self.clone(),
                    offset: slot * XOL_SLOT_SIZE,
                    generation: self.generation,
                });
            }
        }
        None
    }

    fn free_slot(&self, offset: usize, generation: u64) {
        if generation != self.generation {
            return;
        }
        let slot = offset / XOL_SLOT_SIZE;
        if slot < XOL_SLOTS_PER_PAGE {
            self.slot_bitmap.lock_irqsave()[slot / 64] &= !(1u64 << (slot % 64));
        }
    }

    /// 计算 slot 对应的用户虚拟地址（供 batch3 使用）。
    pub fn slot_vaddr(&self, offset: usize) -> VirtAddr {
        VirtAddr::new(self.page_base.data() + offset)
    }

    /// XOL 页基地址（供 batch3 计算 slot 地址）。
    pub fn page_base(&self) -> VirtAddr {
        self.page_base
    }

    /// XOL 页物理地址（供 batch3 在关中断路径下通过 `phys_2_virt` 写 slot 内容）。
    pub fn page_paddr(&self) -> PhysAddr {
        self.page_paddr
    }
}

/// 一个 XOL slot 的唯一所有权租约。命中路径应把 `Arc<XolSlotLease>` 放入
/// `ActiveXol`，从而让注销只撤销后续命中，不能复用仍在执行的 slot。
pub struct XolSlotLease {
    area: Arc<XolArea>,
    offset: usize,
    generation: u64,
}

impl XolSlotLease {
    pub fn offset(&self) -> usize {
        self.offset
    }

    pub fn slot_vaddr(&self) -> VirtAddr {
        self.area.slot_vaddr(self.offset)
    }

    pub fn page_paddr(&self) -> PhysAddr {
        self.area.page_paddr()
    }

    pub fn area(&self) -> &Arc<XolArea> {
        &self.area
    }
}

impl Drop for XolSlotLease {
    fn drop(&mut self) {
        self.area.free_slot(self.offset, self.generation);
    }
}

impl core::fmt::Debug for XolSlotLease {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("XolSlotLease")
            .field("offset", &self.offset)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

static NEXT_XOL_GENERATION: AtomicU64 = AtomicU64::new(1);
