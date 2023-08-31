/*
 * Address types:
 *
 *  gva - guest virtual address
 *  gpa - guest physical address
 *  gfn - guest frame number
 *  hva - host virtual address
 *  hpa - host physical address
 *  hfn - host frame number
 */
pub const KVM_USER_MEM_SLOTS:u32 =  16;
pub const KVM_PRIVATE_MEM_SLOTS:u32 = 3;
pub const KVM_MEM_SLOTS_NUM:u32 = KVM_USER_MEM_SLOTS + KVM_PRIVATE_MEM_SLOTS;
pub const KVM_ADDRESS_SPACE_NUM:usize = 2;

pub const KVM_MEM_LOG_DIRTY_PAGES:u32 = 1 << 0;
pub const KVM_MEM_READONLY:u32 = 1 << 1;
pub const KVM_MEM_MAX_NR_PAGES :u32 = (1 << 31) -1;

pub const PAGE_SHIFT:u32 = 12;
#[repr(C)]
/// 通过这个结构可以将虚拟机的物理地址对应到用户进程的虚拟地址
/// 用来表示虚拟机的一段物理内存
pub struct KvmUserspaceMemoryRegion {
    pub slot: u32, // 要在哪个slot上注册内存区间
    // flags有两个取值，KVM_MEM_LOG_DIRTY_PAGES和KVM_MEM_READONLY，用来指示kvm针对这段内存应该做的事情。
    // KVM_MEM_LOG_DIRTY_PAGES用来开启内存脏页，KVM_MEM_READONLY用来开启内存只读。
    pub flags: u32, 
    pub guest_phys_addr: u64, // 虚机内存区间起始物理地址
    pub memory_size: u64,     // 虚机内存区间大小
    pub userspace_addr: u64,  // 虚机内存区间对应的主机虚拟地址
}

pub struct KvmMemorySlot{
    pub base_gfn: u64, // 虚机内存区间起始物理页框号
    pub npages: u64,   // 虚机内存区间页数，即内存区间的大小
    // 用来记录虚机内存区间的脏页信息，每个bit对应一个页，如果bit为1，表示对应的页是脏页，如果bit为0，表示对应的页是干净页。
    pub dirty_bitmap: *mut u8, 
	// unsigned long *rmap[KVM_NR_PAGE_SIZES]; 反向映射相关的结构, 创建EPT页表项时就记录GPA对应的页表项地址(GPA-->页表项地址)，暂时不需要
    pub userspace_addr: u64,  // 虚机内存区间对应的主机虚拟地址
    pub flags: u32,    // 虚机内存区间属性
    pub id: u16,       // 虚机内存区间id
}

pub struct KvmMemorySlots{
    pub memslots: [KvmMemorySlot; KVM_MEM_SLOTS_NUM as usize], // 虚机内存区间数组
    pub used_slots: u32, // 已经使用的slot数量
}

pub enum KvmMemoryChange{
    Create,
    Delete,
	Move,
	FlagsOnly,
}

impl Default for KvmUserspaceMemoryRegion {
    fn default() -> KvmUserspaceMemoryRegion {
        KvmUserspaceMemoryRegion {
            slot: 0,
            flags: 0,
            guest_phys_addr: 0,
            memory_size: 0,
            userspace_addr: 0,
        }
    }
}

