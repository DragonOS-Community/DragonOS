/// Linux scheduling policy implemented by DragonOS.
///
/// This is the task's base policy. Flags such as `SCHED_RESET_ON_FORK` and the
/// scheduler's effective class are deliberately stored separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum LinuxSchedPolicy {
    Normal = 0,
    Fifo = 1,
    Rr = 2,
}

impl LinuxSchedPolicy {
    /// Return the policy's class when no priority-inheritance override applies.
    #[inline]
    pub const fn base_sched_class(self) -> SchedClass {
        match self {
            Self::Normal => SchedClass::Fair,
            Self::Fifo | Self::Rr => SchedClass::Realtime,
        }
    }

    #[inline]
    pub const fn is_realtime(self) -> bool {
        matches!(self, Self::Fifo | Self::Rr)
    }

    #[inline]
    pub const fn to_u8(self) -> u8 {
        self as u8
    }

    #[inline]
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Normal,
            1 => Self::Fifo,
            2 => Self::Rr,
            _ => panic!("invalid Linux scheduling policy"),
        }
    }
}

/// Scheduler implementation currently responsible for a task.
///
/// Unlike [`LinuxSchedPolicy`], this is an effective property: a future
/// priority-inheritance implementation may temporarily move a task to a class
/// other than its base policy's class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SchedClass {
    Realtime = 0,
    Fair = 1,
    Idle = 2,
}

impl SchedClass {
    #[inline]
    pub const fn outranks(self, other: Self) -> bool {
        matches!(
            (self, other),
            (Self::Realtime, Self::Fair | Self::Idle) | (Self::Fair, Self::Idle)
        )
    }

    #[inline]
    pub const fn to_u8(self) -> u8 {
        self as u8
    }

    #[inline]
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Realtime,
            1 => Self::Fair,
            2 => Self::Idle,
            _ => panic!("invalid scheduler class"),
        }
    }
}

const _: () = {
    assert!(LinuxSchedPolicy::Normal.base_sched_class() as u8 == SchedClass::Fair as u8);
    assert!(LinuxSchedPolicy::Fifo.base_sched_class() as u8 == SchedClass::Realtime as u8);
    assert!(LinuxSchedPolicy::Rr.base_sched_class() as u8 == SchedClass::Realtime as u8);
    assert!(SchedClass::Realtime.outranks(SchedClass::Fair));
    assert!(SchedClass::Realtime.outranks(SchedClass::Idle));
    assert!(SchedClass::Fair.outranks(SchedClass::Idle));
    assert!(!SchedClass::Fair.outranks(SchedClass::Realtime));
    assert!(!SchedClass::Idle.outranks(SchedClass::Fair));
    assert!(!SchedClass::Idle.outranks(SchedClass::Realtime));
};
