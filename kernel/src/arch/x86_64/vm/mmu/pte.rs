bitflags::bitflags! {
    pub struct PteFlags: u64 {
        const PRESENT = 1 << 0;
        const READ_WRITE = 1 << 1;
        const USER_SUPERVISOR = 1 << 2;
        const PAGE_WRITE_THROUGH = 1 << 3;
        const PAGE_CACHE_DISABLE = 1 << 4;
        const ACCESSED = 1 << 5;
        const DIRTY = 1 << 6;
        const PAGE_SIZE = 1 << 7;
        const GLOBAL = 1 << 8;
        const EXECUTE_DISABLE = 1 << 63;
    }
}

pub struct Pte {
    pub address: u64, // 物理地址
    pub flags: PteFlags, // 页表条目标志
}

impl Pte {
    pub fn new(address: u64, flags: PteFlags) -> Self {
        Self { address, flags }
    }

    pub fn is_present(&self) -> bool {
        self.flags.contains(PteFlags::PRESENT)
    }

    pub fn is_read_write(&self) -> bool {
        self.flags.contains(PteFlags::READ_WRITE)
    }

    pub fn is_user_supervisor(&self) -> bool {
        self.flags.contains(PteFlags::USER_SUPERVISOR)
    }

    pub fn is_executable(&self) -> bool {
        !self.flags.contains(PteFlags::EXECUTE_DISABLE)
    }

    // 其他方法...
}