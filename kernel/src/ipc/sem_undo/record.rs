//! Live undo debt and allocation reservations prepared before taking the manager lock.
use super::SemUndoGroup;
use crate::ipc::sem::SemId;
use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
};

#[derive(Debug)]
pub(super) struct PendingSemUndoRecordReservation {
    group: Weak<SemUndoGroup>,
    active: bool,
}

#[derive(Debug)]
pub(crate) struct SemUndoRecord {
    pub(super) semid: SemId,
    pub(super) adjustments: Box<[i16]>,
}

/// Existing records need no snapshot: simulation reads the current record
/// while holding the group lock. Only a first-use candidate owns dense storage.
#[derive(Debug)]
pub(crate) struct PreparedSemUndoRecord {
    pub(super) semid: SemId,
    pub(super) nsems: usize,
    pub(super) candidate: Option<SemUndoRecord>,
    pub(super) reservation: Option<PendingSemUndoRecordReservation>,
}

pub(crate) enum PreparedSemUndoRecordAction<R> {
    Complete(R),
    Keep(R),
}

impl PendingSemUndoRecordReservation {
    pub(super) fn new(group: &Arc<SemUndoGroup>) -> Self {
        Self {
            group: Arc::downgrade(group),
            active: true,
        }
    }

    pub(super) fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for PendingSemUndoRecordReservation {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let Some(group) = self.group.upgrade() else {
            return;
        };
        let mut state = group.inner.lock_irqsave();
        state.reserved_records = state
            .reserved_records
            .checked_sub(1)
            .expect("SEM_UNDO record reservation count underflow");
        self.active = false;
    }
}

impl PreparedSemUndoRecord {
    pub(crate) fn was_existing(&self) -> bool {
        self.candidate.is_none()
    }

    pub(crate) fn adjustment_count(&self) -> usize {
        self.nsems
    }
}

impl SemUndoRecord {
    #[cfg(test)]
    pub(super) fn new_live(semid: SemId, adjustments: Box<[i16]>) -> Self {
        Self { semid, adjustments }
    }

    pub(crate) fn adjustment(&self, semnum: usize) -> i16 {
        self.adjustments[semnum]
    }

    pub(crate) fn set_adjustment(&mut self, semnum: usize, adjustment: i16) {
        if self.adjustments[semnum] != adjustment {
            self.adjustments[semnum] = adjustment;
        }
    }

    pub(crate) fn clear_adjustment(&mut self, semnum: usize) {
        self.set_adjustment(semnum, 0);
    }

    pub(crate) fn clear_all_adjustments(&mut self) {
        if self.adjustments.iter().any(|&adjustment| adjustment != 0) {
            self.adjustments.fill(0);
        }
    }

    pub(crate) fn adjustment_count(&self) -> usize {
        self.adjustments.len()
    }

    #[cfg(test)]
    pub(crate) fn adjustment_for_test(&self, semnum: usize) -> i16 {
        self.adjustment(semnum)
    }

    #[cfg(test)]
    pub(crate) fn set_adjustment_for_test(&mut self, semnum: usize, adjustment: i16) {
        self.set_adjustment(semnum, adjustment);
    }
}
