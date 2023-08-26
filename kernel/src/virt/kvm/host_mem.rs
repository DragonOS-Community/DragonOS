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
pub const KVM_USER_MEM_SLOTS:usize =  16;
pub const KVM_PRIVATE_MEM_SLOTS:usize = 3;
pub const KVM_MEM_SLOTS_NUM:usize = KVM_USER_MEM_SLOTS + KVM_PRIVATE_MEM_SLOTS;

struct KvmUserspaceMemoryRegion {
    slot: u32, // 要在哪个slot上注册内存区间
    // flags有两个取值，KVM_MEM_LOG_DIRTY_PAGES和KVM_MEM_READONLY，用来指示kvm针对这段内存应该做的事情。
    // KVM_MEM_LOG_DIRTY_PAGES用来开启内存脏页，KVM_MEM_READONLY用来开启内存只读。
    flags: u32, 
    guest_phys_addr: u64, // 虚机内存区间起始物理地址
    memory_size: u64,     // 虚机内存区间大小
    userspace_addr: u64,  // 虚机内存区间对应的主机虚拟地址
}

struct KvmMemorySlot{
    base_gfn: u64, // 虚机内存区间起始物理页框号
    npages: u64,   // 虚机内存区间页数，即内存区间的大小
    // 用来记录虚机内存区间的脏页信息，每个bit对应一个页，如果bit为1，表示对应的页是脏页，如果bit为0，表示对应的页是干净页。
    dirty_bitmap: *mut u64, 
	// unsigned long *rmap[KVM_NR_PAGE_SIZES]; 反向映射相关的结构, 创建EPT页表项时就记录GPA对应的页表项地址(GPA-->页表项地址)，暂时不需要
    userspace_addr: u64,  // 虚机内存区间对应的主机虚拟地址
    flags: u32,    // 虚机内存区间属性
    id: u16,       // 虚机内存区间id
}

struct KvmMemorySlots{
    memslots: [KvmMemorySlot; KVM_MEM_SLOTS_NUM], // 虚机内存区间数组
    used_slots: u32, // 已经使用的slot数量
}