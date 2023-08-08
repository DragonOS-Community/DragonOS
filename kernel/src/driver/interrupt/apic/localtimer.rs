use crate::syscall::SystemError;

// Import interrupt-related functions and types
use crate::interrupt::InterruptArch;
use crate::interrupt::{IrqFlags, IrqFlagsGuard};

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ApicTimerMode {
    OneShot,
    Periodic,
    TSCDeadline,
}

impl ApicTimerMode {
    pub fn start_timer(&self, duration: u64) {
        
        // Start the timer based on the mode
        match self {
            ApicTimerMode::OneShot => self.start_oneshot_timer(duration),
            ApicTimerMode::Periodic => self.start_periodic_timer(duration),
            ApicTimerMode::TSCDeadline => self.start_tsc_deadline_timer(duration),
        }

        // The timer is running...

        // Interrupts will be automatically restored when `irq_guard` is dropped
    }

    fn start_oneshot_timer(&self, duration: u64) {
        // Set the timer duration
        // ...

        // Start the timer
        // ...
    }

    fn start_periodic_timer(&self, duration: u64) {
        // Set the timer duration
        // ...

        // Start the timer
        // ...
    }

    fn start_tsc_deadline_timer(&self, duration: u64) {
        // Set the TSC deadline value
        // ...

        // Start the timer
        // ...
    }
}

impl TryFrom<u8> for ApicTimerMode {
    type Error = SystemError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0b00 => Ok(ApicTimerMode::OneShot),
            0b01 => Ok(ApicTimerMode::Periodic),
            0b10 => Ok(ApicTimerMode::TSCDeadline),
            _ => Err(SystemError::EINVAL),
        }
    }
}
