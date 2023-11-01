bitflags! {
    pub struct FutexArg: u32 {
        const FUTEX_WAIT = 0;
        const FUTEX_WAKE = 1;
        const FUTEX_FD = 2;
        const FUTEX_REQUEUE = 3;
        const FUTEX_CMP_REQUEUE = 4;
        const FUTEX_WAKE_OP = 5;
        const FUTEX_LOCK_PI = 6;
        const FUTEX_UNLOCK_PI = 7;
        const FUTEX_TRYLOCK_PI = 8;
        const FUTEX_WAIT_BITSET = 9;
        const FUTEX_WAKE_BITSET = 10;
        const FUTEX_WAIT_REQUEUE_PI = 11;
        const FUTEX_CMP_REQUEUE_PI = 12;
        const FUTEX_LOCK_PI2 = 13;
    }

    pub struct FutexFlag: u32 {
        const FLAGS_MATCH_NONE = 0x01;
        const FLAGS_SHARED = 0x01;
        const FLAGS_CLOCKRT = 0x02;
        const FLAGS_HAS_TIMEOUT = 0x04;
        const FUTEX_PRIVATE_FLAG = 128;
        const FUTEX_CLOCK_REALTIME = 256;
        const FUTEX_CMD_MASK = !(Self::FUTEX_PRIVATE_FLAG.bits() | Self::FUTEX_CLOCK_REALTIME.bits());
    }

    pub struct FutexOP: u32 {
        const FUTEX_OP_SET = 0;
        const FUTEX_OP_ADD = 1;
        const FUTEX_OP_OR = 2;
        const FUTEX_OP_ANDN = 3;
        const FUTEX_OP_XOR = 4;
        const FUTEX_OP_OPARG_SHIFT = 8;
    }

    pub struct FutexOpCMP: u32 {
        const FUTEX_OP_CMP_EQ = 0;
        const FUTEX_OP_CMP_NE = 1;
        const FUTEX_OP_CMP_LT = 2;
        const FUTEX_OP_CMP_LE = 3;
        const FUTEX_OP_CMP_GT = 4;
        const FUTEX_OP_CMP_GE = 5;
    }
}

#[allow(dead_code)]
pub const FUTEX_WAITERS: u32 = 0x80000000;
#[allow(dead_code)]
pub const FUTEX_OWNER_DIED: u32 = 0x40000000;
#[allow(dead_code)]
pub const FUTEX_TID_MASK: u32 = 0x3fffffff;
pub const FUTEX_BITSET_MATCH_ANY: u32 = 0xffffffff;
