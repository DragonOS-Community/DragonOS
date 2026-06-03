use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use system_error::SystemError;

use crate::{
    libs::spinlock::SpinLock,
    process::{
        cred::{capable, CAPFlags},
        ProcessManager,
    },
};

pub const IO_BITMAP_BITS: usize = 65_536;
pub const IO_BITMAP_BYTES: usize = IO_BITMAP_BITS / 8;
pub const IO_BITMAP_TERMINATOR_BYTES: usize = 1;

static IO_BITMAP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn next_sequence() -> u64 {
    IO_BITMAP_SEQUENCE.fetch_add(1, Ordering::Relaxed) + 1
}

#[derive(Debug, Clone)]
pub struct TaskIoBitmap {
    sequence: u64,
    max_bytes: usize,
    bitmap: [u8; IO_BITMAP_BYTES],
}

impl TaskIoBitmap {
    pub fn new_all_denied() -> Self {
        Self {
            sequence: next_sequence(),
            max_bytes: 0,
            bitmap: [0xff; IO_BITMAP_BYTES],
        }
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    pub fn bitmap(&self) -> &[u8; IO_BITMAP_BYTES] {
        &self.bitmap
    }

    pub fn allow_range(&mut self, from: usize, num: usize) {
        self.update_range(from, num, false);
    }

    pub fn deny_range(&mut self, from: usize, num: usize) {
        self.update_range(from, num, true);
    }

    pub fn all_denied(&self) -> bool {
        self.max_bytes == 0
    }

    fn update_range(&mut self, from: usize, num: usize, deny: bool) {
        let end = from + num;
        for port in from..end {
            let byte = port / 8;
            let bit = 1u8 << (port & 7);
            if deny {
                self.bitmap[byte] |= bit;
            } else {
                self.bitmap[byte] &= !bit;
            }
        }
        self.recompute_max_bytes();
        self.sequence = next_sequence();
    }

    fn recompute_max_bytes(&mut self) {
        self.max_bytes = self
            .bitmap
            .iter()
            .rposition(|byte| *byte != 0xff)
            .map(|index| index + 1)
            .unwrap_or(0);
    }
}

fn validate_range(from: usize, num: usize) -> Result<(), SystemError> {
    let end = from.checked_add(num).ok_or(SystemError::EINVAL)?;
    if end <= from || end > IO_BITMAP_BITS {
        return Err(SystemError::EINVAL);
    }
    Ok(())
}

pub fn do_ioperm(from: usize, num: usize, turn_on: bool) -> Result<usize, SystemError> {
    validate_range(from, num)?;

    if turn_on && !capable(CAPFlags::CAP_SYS_RAWIO) {
        return Err(SystemError::EPERM);
    }

    let current = ProcessManager::current_pcb();
    let bitmap = loop {
        let shared_bitmap = {
            let mut arch = current.arch_info_irqsave();

            if arch.io_bitmap_ref().is_none() {
                if !turn_on {
                    return Ok(0);
                }
                arch.set_io_bitmap(Some(Arc::new(
                    SpinLock::new(TaskIoBitmap::new_all_denied()),
                )));
            }

            let existing = arch
                .io_bitmap_ref()
                .expect("ioperm bitmap must exist after allocation");
            if Arc::strong_count(existing) == 1 {
                break arch
                    .io_bitmap()
                    .expect("ioperm bitmap must exist after allocation");
            }
            arch.io_bitmap()
                .expect("ioperm bitmap must exist after allocation")
        };

        let cloned = {
            let guard = shared_bitmap.lock_irqsave();
            guard.clone()
        };
        let private_bitmap = Arc::new(SpinLock::new(cloned));

        let mut arch = current.arch_info_irqsave();
        if arch
            .io_bitmap_ref()
            .map(|existing| Arc::ptr_eq(existing, &shared_bitmap))
            .unwrap_or(false)
        {
            arch.set_io_bitmap(Some(private_bitmap.clone()));
            break private_bitmap;
        }
    };

    let all_denied = {
        let mut guard = bitmap.lock_irqsave();
        if turn_on {
            guard.allow_range(from, num);
        } else {
            guard.deny_range(from, num);
        }
        guard.all_denied()
    };

    if all_denied {
        let mut arch = current.arch_info_irqsave();
        if arch
            .io_bitmap_ref()
            .map(|existing| Arc::ptr_eq(existing, &bitmap))
            .unwrap_or(false)
        {
            arch.set_io_bitmap(None);
        }
    }

    Ok(0)
}
