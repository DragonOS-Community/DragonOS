pub use self::define::*;

#[allow(dead_code)]
pub mod define {
    pub const FUTEX_WAIT: u32 = 0;
    pub const FUTEX_WAKE: u32 = 1;
    pub const FUTEX_FD: u32 = 2;
    pub const FUTEX_REQUEUE: u32 = 3;
    pub const FUTEX_CMP_REQUEUE: u32 = 4;
    pub const FUTEX_WAKE_OP: u32 = 5;
    pub const FUTEX_LOCK_PI: u32 = 6;
    pub const FUTEX_UNLOCK_PI: u32 = 7;
    pub const FUTEX_TRYLOCK_PI: u32 = 8;
    pub const FUTEX_WAIT_BITSET: u32 = 9;
    pub const FUTEX_WAKE_BITSET: u32 = 10;
    pub const FUTEX_WAIT_REQUEUE_PI: u32 = 11;
    pub const FUTEX_CMP_REQUEUE_PI: u32 = 12;
    pub const FUTEX_LOCK_PI2: u32 = 13;

    pub const FLAGS_SHARED: u32 = 0x01;
    pub const FLAGS_CLOCKRT: u32 = 0x02;
    pub const FLAGS_HAS_TIMEOUT: u32 = 0x04;
    pub const FUTEX_PRIVATE_FLAG: u32 = 128;
    pub const FUTEX_CLOCK_REALTIME: u32 = 256;

    pub const FUTEX_WAITERS: u32 = 0x80000000;
    pub const FUTEX_OWNER_DIED: u32 = 0x40000000;

    pub const FUTEX_OP_SET: u32 = 0;
    pub const FUTEX_OP_ADD: u32 = 1;
    pub const FUTEX_OP_OR: u32 = 2;
    pub const FUTEX_OP_ANDN: u32 = 3;
    pub const FUTEX_OP_XOR: u32 = 4;

    pub const FUTEX_OP_OPARG_SHIFT: u32 = 8;

    pub const FUTEX_OP_CMP_EQ: u32 = 0;
    pub const FUTEX_OP_CMP_NE: u32 = 1;
    pub const FUTEX_OP_CMP_LT: u32 = 2;
    pub const FUTEX_OP_CMP_LE: u32 = 3;
    pub const FUTEX_OP_CMP_GT: u32 = 4;
    pub const FUTEX_OP_CMP_GE: u32 = 5;

    pub const FUTEX_TID_MASK: u32 = 0x3fffffff;
    pub const FUTEX_BITSET_MATCH_ANY: u32 = 0xffffffff;
    pub const FUTEX_CMD_MASK: u32 = !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);
}
