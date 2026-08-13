use super::{
    AllocationClass, AllocationState, DelallocClaim, DelallocConsumptionRecord, DelallocLease,
    DelallocReservation, DelallocReservationId, DelallocReservationUse, Ext4,
};
use crate::constants::*;
use crate::ext4_defs::*;
use crate::format_error;
use crate::prelude::*;
use crate::return_error;

pub(super) struct RangeAllocation {
    pub first: PBlockId,
    pub allocation_homes: [PBlockId; 3],
}

struct TransactionRangeAllocationRequest {
    inode_id: InodeId,
    preferred_first: Option<PBlockId>,
    require_preferred: bool,
    count: u32,
    class: AllocationClass,
    reservation_use: DelallocReservationUse,
    probe_limit: u32,
}

/// A one-shot debit made while the caller holds `alloc_lock`.  It must be
/// explicitly committed after the associated bitmap/transaction update becomes
/// owned by the reservation, or rolled back before dropping that lock when the
/// update aborts.  This intentionally is neither `Copy` nor `Clone`: a debit
/// must never be credited back twice.
///
/// The type and its `resolve` operation are deliberately private to this
/// module.  Future mappers may receive it only through an allocation helper;
/// they must not be able to disarm the fail-stop check without atomically
/// updating `AllocationState` through commit or rollback below.
#[cfg_attr(not(test), allow(dead_code))]
#[must_use = "a delayed-allocation debit must be explicitly committed or rolled back"]
#[derive(Debug, PartialEq, Eq)]
pub(super) struct DelallocConsumption {
    mount_generation: u64,
    serial: u64,
    unresolved: bool,
}

impl DelallocConsumption {
    pub(super) fn resolve(&mut self) {
        self.unresolved = false;
    }
}

impl Drop for DelallocConsumption {
    fn drop(&mut self) {
        assert!(
            !self.unresolved,
            "Delayed-allocation consumption was dropped without commit or rollback"
        );
    }
}

// Range allocation runs under the transaction writer gate.  Keep the
// speculative search short so a fragmented filesystem cannot turn a single
// sequential write into a volume-wide metadata read.
const TRANSACTION_RANGE_MAX_GROUP_PROBES: u32 = 4;

#[cfg_attr(not(test), allow(dead_code))]
impl AllocationState {
    fn total_reserved_blocks(&self) -> Result<u64> {
        self.reserved_data_blocks
            .checked_add(self.reserved_metadata_blocks)
            .ok_or_else(|| {
                format_error!(
                    ErrCode::EIO,
                    "Delayed-allocation reservation accounting overflowed"
                )
            })
    }

    /// Check whether a normal eager allocation can spend `blocks` without
    /// consuming space promised to delayed writeback.  The caller holds
    /// `alloc_lock`, which also serializes bitmap and free-counter changes.
    fn ensure_unreserved_capacity(&self, free_blocks: u64, blocks: u64) -> Result<()> {
        let reserved = self.total_reserved_blocks()?;
        let available = free_blocks.checked_sub(reserved).ok_or_else(|| {
            format_error!(
                ErrCode::EIO,
                "Free-block counter is smaller than outstanding delayed-allocation claims"
            )
        })?;
        if blocks > available {
            return_error!(
                ErrCode::ENOSPC,
                "Insufficient unreserved space: {} blocks requested with {} blocks reserved",
                blocks,
                reserved
            );
        }
        Ok(())
    }

    fn reserve_delalloc(
        &mut self,
        free_blocks: u64,
        data_blocks: u64,
        metadata_blocks: u64,
    ) -> Result<DelallocReservation> {
        let requested = data_blocks.checked_add(metadata_blocks).ok_or_else(|| {
            format_error!(ErrCode::ERANGE, "Delayed-allocation reservation overflows")
        })?;
        if requested == 0 {
            return_error!(
                ErrCode::EINVAL,
                "Delayed-allocation reservation must contain data or metadata blocks"
            );
        }
        self.ensure_unreserved_capacity(free_blocks, requested)?;

        let serial = self.next_delalloc_reservation_serial;
        let next_serial = serial.checked_add(1).ok_or_else(|| {
            format_error!(
                ErrCode::ERANGE,
                "Delayed-allocation reservation identifiers are exhausted"
            )
        })?;
        let new_data_total = self
            .reserved_data_blocks
            .checked_add(data_blocks)
            .ok_or_else(|| format_error!(ErrCode::ERANGE, "Data reservation counter overflows"))?;
        let new_metadata_total = self
            .reserved_metadata_blocks
            .checked_add(metadata_blocks)
            .ok_or_else(|| {
                format_error!(ErrCode::ERANGE, "Metadata reservation counter overflows")
            })?;

        let id = DelallocReservationId {
            mount_generation: self.mount_generation,
            serial,
        };
        // `serial` is monotonically increasing and zero is never issued, so
        // an insertion collision would prove memory corruption rather than a
        // recoverable allocation miss.  The mount generation also prevents a
        // handle from one Ext4 instance matching a claim in another instance.
        if self
            .delalloc_claims
            .insert(
                id,
                DelallocClaim {
                    data_blocks,
                    metadata_blocks,
                    inflight_consumptions: 0,
                },
            )
            .is_some()
        {
            return_error!(ErrCode::EIO, "Duplicate delayed-allocation reservation id");
        }
        self.next_delalloc_reservation_serial = next_serial;
        self.reserved_data_blocks = new_data_total;
        self.reserved_metadata_blocks = new_metadata_total;
        Ok(DelallocReservation {
            id,
            data_blocks,
            metadata_blocks,
            active: true,
            append_block_certificate: None,
        })
    }

    /// Release a set of whole, unconsumed reservations as one ledger update.
    ///
    /// A future per-inode truncate bridge cannot release reservations by
    /// iterating geometric tail fragments: a fragment does not carry a
    /// proportionate metadata claim, and a failure halfway through would
    /// leave the inode queue and global ledger disagreeing. This helper first
    /// canonicalises the opaque leases by identity, validates every lease, and
    /// then performs only non-allocating removals and counter updates. The
    /// VFS cannot inspect an identity, so it must not be responsible for
    /// satisfying an identity-order precondition. Sorting the caller-provided
    /// slice needs no allocation; the subsequent strict check still catches a
    /// duplicate/corrupt identity without taking an O(n²) path under
    /// `alloc_lock`. An empty batch is an intentional no-op. No eager
    /// allocator can observe an intermediate capacity state.
    fn release_delalloc_batch(
        &mut self,
        reservations: &mut [&mut DelallocReservation],
    ) -> Result<()> {
        reservations.sort_unstable_by_key(|reservation| reservation.id);
        let mut released_data_blocks = 0u64;
        let mut released_metadata_blocks = 0u64;
        let mut previous_id = None;

        for reservation in reservations.iter() {
            let reservation: &DelallocReservation = reservation;
            if !reservation.active {
                return_error!(
                    ErrCode::EINVAL,
                    "Delayed-allocation lease was already released or finalised"
                );
            }
            if reservation.id.mount_generation != self.mount_generation {
                return_error!(
                    ErrCode::EINVAL,
                    "Delayed-allocation reservation belongs to another filesystem instance"
                );
            }
            if previous_id.is_some_and(|previous| previous >= reservation.id) {
                return_error!(
                    ErrCode::EINVAL,
                    "Delayed-allocation reservation batch contains a duplicate identity"
                );
            }
            previous_id = Some(reservation.id);
            let claim = self.delalloc_claims.get(&reservation.id).ok_or_else(|| {
                format_error!(
                    ErrCode::EINVAL,
                    "Unknown or already released delayed-allocation reservation"
                )
            })?;
            if claim.inflight_consumptions != 0 {
                return_error!(
                    ErrCode::EIO,
                    "Cannot release a delayed-allocation reservation with an unfinished allocation"
                );
            }
            // This is the bridge for deleting complete pending-map entries,
            // not a generic "release whatever remains" operation.  A
            // committed debit means that part of this entry has already
            // materialised.  Releasing the residual claim would make a
            // malformed queue state silently discard the remainder of a
            // reservation.  Partial cancellation needs a separately proved
            // split/remaining-claim protocol; it must not reuse this whole
            // entry primitive.
            if claim.data_blocks != reservation.data_blocks
                || claim.metadata_blocks != reservation.metadata_blocks
            {
                return_error!(
                    ErrCode::EIO,
                    "Cannot release a delayed-allocation reservation that was partially materialised"
                );
            }
            released_data_blocks = released_data_blocks
                .checked_add(claim.data_blocks)
                .ok_or_else(|| {
                    format_error!(
                        ErrCode::EIO,
                        "Delayed-allocation batch data release accounting overflowed"
                    )
                })?;
            released_metadata_blocks = released_metadata_blocks
                .checked_add(claim.metadata_blocks)
                .ok_or_else(|| {
                    format_error!(
                        ErrCode::EIO,
                        "Delayed-allocation batch metadata release accounting overflowed"
                    )
                })?;
        }

        let new_reserved_data_blocks = self
            .reserved_data_blocks
            .checked_sub(released_data_blocks)
            .ok_or_else(|| {
                format_error!(
                    ErrCode::EIO,
                    "Delayed-allocation data reservation accounting underflowed"
                )
            })?;
        let new_reserved_metadata_blocks = self
            .reserved_metadata_blocks
            .checked_sub(released_metadata_blocks)
            .ok_or_else(|| {
                format_error!(
                    ErrCode::EIO,
                    "Delayed-allocation metadata reservation accounting underflowed"
                )
            })?;

        for reservation in reservations.iter_mut() {
            let removed = self.delalloc_claims.remove(&reservation.id);
            debug_assert!(removed.is_some());
            reservation.deactivate();
        }
        self.reserved_data_blocks = new_reserved_data_blocks;
        self.reserved_metadata_blocks = new_reserved_metadata_blocks;
        Ok(())
    }

    fn release_delalloc(&mut self, reservation: &mut DelallocReservation) -> Result<()> {
        self.release_delalloc_batch(&mut [reservation])
    }

    /// Debit an already-reserved claim after its physical allocation has been
    /// staged.  The caller keeps `alloc_lock` held across the allocation and
    /// this debit, so an eager allocator cannot spend the just-released budget
    /// before the bitmap change becomes visible.
    fn consume_delalloc(
        &mut self,
        class: AllocationClass,
        use_kind: DelallocReservationUse,
        blocks: u64,
    ) -> Result<Option<DelallocConsumption>> {
        if blocks == 0 {
            return_error!(ErrCode::EINVAL, "Cannot consume an empty allocation claim");
        }
        let AllocationClass::Delalloc(reservation) = class else {
            return Ok(None);
        };
        if reservation.mount_generation != self.mount_generation {
            return_error!(
                ErrCode::EINVAL,
                "Delayed-allocation reservation belongs to another filesystem instance"
            );
        }
        let consumption_serial = self.next_delalloc_consumption_serial;
        let next_consumption_serial = consumption_serial.checked_add(1).ok_or_else(|| {
            format_error!(
                ErrCode::ERANGE,
                "Delayed-allocation consumption identifiers are exhausted"
            )
        })?;
        if self.delalloc_consumptions.contains_key(&consumption_serial) {
            return_error!(ErrCode::EIO, "Duplicate delayed-allocation consumption id");
        }

        let claim = self.delalloc_claims.get(&reservation).ok_or_else(|| {
            format_error!(ErrCode::EINVAL, "Unknown delayed-allocation reservation")
        })?;
        let remaining = match use_kind {
            DelallocReservationUse::Data => claim.data_blocks,
            DelallocReservationUse::Metadata => claim.metadata_blocks,
        };
        if remaining < blocks {
            // A successful front-end reservation must make this impossible.
            // It is an invariant violation, not a late ENOSPC that writeback
            // may quietly convert into a retry.
            return_error!(
                ErrCode::EIO,
                "Delayed-allocation reservation was consumed beyond its claim"
            );
        }
        let new_remaining = remaining - blocks;
        let new_inflight_consumptions =
            claim.inflight_consumptions.checked_add(1).ok_or_else(|| {
                format_error!(
                    ErrCode::EIO,
                    "Delayed-allocation in-flight consumption counter overflowed"
                )
            })?;
        let new_reserved_total = match use_kind {
            DelallocReservationUse::Data => self
                .reserved_data_blocks
                .checked_sub(blocks)
                .ok_or_else(|| {
                    format_error!(
                        ErrCode::EIO,
                        "Delayed-allocation data reservation accounting underflowed"
                    )
                })?,
            DelallocReservationUse::Metadata => self
                .reserved_metadata_blocks
                .checked_sub(blocks)
                .ok_or_else(|| {
                format_error!(
                    ErrCode::EIO,
                    "Delayed-allocation metadata reservation accounting underflowed"
                )
            })?,
        };
        let old_consumption = self.delalloc_consumptions.insert(
            consumption_serial,
            DelallocConsumptionRecord {
                reservation,
                use_kind,
                blocks,
            },
        );
        debug_assert!(old_consumption.is_none());
        let claim = self
            .delalloc_claims
            .get_mut(&reservation)
            .expect("delayed-allocation claim was validated before consumption");
        match use_kind {
            DelallocReservationUse::Data => claim.data_blocks = new_remaining,
            DelallocReservationUse::Metadata => claim.metadata_blocks = new_remaining,
        }
        claim.inflight_consumptions = new_inflight_consumptions;
        match use_kind {
            DelallocReservationUse::Data => self.reserved_data_blocks = new_reserved_total,
            DelallocReservationUse::Metadata => {
                self.reserved_metadata_blocks = new_reserved_total;
            }
        }
        self.next_delalloc_consumption_serial = next_consumption_serial;
        Ok(Some(DelallocConsumption {
            mount_generation: self.mount_generation,
            serial: consumption_serial,
            unresolved: true,
        }))
    }

    /// Undo a debit when the allocation which was about to consume it failed
    /// before it became owned by the delayed mapping.  The same `alloc_lock`
    /// critical section must cover the failed bitmap/transaction rollback and
    /// this call.
    fn rollback_delalloc_consumption(
        &mut self,
        mut consumption: DelallocConsumption,
    ) -> Result<()> {
        let result = (|| {
            if consumption.mount_generation != self.mount_generation {
                return_error!(
                    ErrCode::EINVAL,
                    "Delayed-allocation consumption belongs to another filesystem instance"
                );
            }
            let consumption_record = *self
                .delalloc_consumptions
                .get(&consumption.serial)
                .ok_or_else(|| {
                    format_error!(
                        ErrCode::EINVAL,
                        "Unknown or already finalised delayed-allocation consumption"
                    )
                })?;
            let claim = self
                .delalloc_claims
                .get(&consumption_record.reservation)
                .ok_or_else(|| {
                    format_error!(
                        ErrCode::EIO,
                        "Delayed-allocation claim disappeared before rollback"
                    )
                })?;
            let (new_claim_total, new_reserved_total) = match consumption_record.use_kind {
                DelallocReservationUse::Data => {
                    let new_claim = claim
                        .data_blocks
                        .checked_add(consumption_record.blocks)
                        .ok_or_else(|| {
                            format_error!(ErrCode::EIO, "Data reservation rollback overflowed")
                        })?;
                    let new_reserved = self
                        .reserved_data_blocks
                        .checked_add(consumption_record.blocks)
                        .ok_or_else(|| {
                            format_error!(ErrCode::EIO, "Data reservation rollback overflowed")
                        })?;
                    (new_claim, new_reserved)
                }
                DelallocReservationUse::Metadata => {
                    let new_claim = claim
                        .metadata_blocks
                        .checked_add(consumption_record.blocks)
                        .ok_or_else(|| {
                            format_error!(ErrCode::EIO, "Metadata reservation rollback overflowed")
                        })?;
                    let new_reserved = self
                        .reserved_metadata_blocks
                        .checked_add(consumption_record.blocks)
                        .ok_or_else(|| {
                            format_error!(ErrCode::EIO, "Metadata reservation rollback overflowed")
                        })?;
                    (new_claim, new_reserved)
                }
            };
            let new_inflight_consumptions =
                claim.inflight_consumptions.checked_sub(1).ok_or_else(|| {
                    format_error!(
                        ErrCode::EIO,
                        "Delayed-allocation in-flight consumption counter underflowed"
                    )
                })?;
            let removed = self.delalloc_consumptions.remove(&consumption.serial);
            debug_assert!(removed.is_some());
            let claim = self
                .delalloc_claims
                .get_mut(&consumption_record.reservation)
                .expect("delayed-allocation claim was validated before rollback");
            match consumption_record.use_kind {
                DelallocReservationUse::Data => {
                    claim.data_blocks = new_claim_total;
                    self.reserved_data_blocks = new_reserved_total;
                }
                DelallocReservationUse::Metadata => {
                    claim.metadata_blocks = new_claim_total;
                    self.reserved_metadata_blocks = new_reserved_total;
                }
            }
            claim.inflight_consumptions = new_inflight_consumptions;
            consumption.resolve();
            Ok(())
        })();
        if result.is_err() {
            // The owning mount will fail-stop on this ledger corruption.  Do
            // not turn the reportable EIO into a second panic while unwinding
            // the one-shot debit.
            consumption.resolve();
        }
        result
    }

    /// Finalise a debit after its physical allocation and owning transaction
    /// have both become irrevocable.  Keeping this explicit turns a dropped
    /// error-path token into a detectable reservation-finalisation failure.
    fn commit_delalloc_consumption(&mut self, mut consumption: DelallocConsumption) -> Result<()> {
        let result = (|| {
            if consumption.mount_generation != self.mount_generation {
                return_error!(
                    ErrCode::EINVAL,
                    "Delayed-allocation consumption belongs to another filesystem instance"
                );
            }
            let consumption_record = *self
                .delalloc_consumptions
                .get(&consumption.serial)
                .ok_or_else(|| {
                    format_error!(
                        ErrCode::EINVAL,
                        "Unknown or already finalised delayed-allocation consumption"
                    )
                })?;
            let new_inflight_consumptions = self
                .delalloc_claims
                .get(&consumption_record.reservation)
                .ok_or_else(|| {
                    format_error!(
                        ErrCode::EIO,
                        "Delayed-allocation claim disappeared before consumption commit"
                    )
                })?
                .inflight_consumptions
                .checked_sub(1)
                .ok_or_else(|| {
                    format_error!(
                        ErrCode::EIO,
                        "Delayed-allocation in-flight consumption counter underflowed"
                    )
                })?;
            let removed = self.delalloc_consumptions.remove(&consumption.serial);
            debug_assert!(removed.is_some());
            let claim = self
                .delalloc_claims
                .get_mut(&consumption_record.reservation)
                .expect("delayed-allocation claim was validated before consumption commit");
            claim.inflight_consumptions = new_inflight_consumptions;
            consumption.resolve();
            Ok(())
        })();
        if result.is_err() {
            consumption.resolve();
        }
        result
    }

    /// Complete an exhausted reservation only after every materialisation debit
    /// has been committed.  Accepting the unique reservation object, instead
    /// of a bare copyable id, prevents a mapping-class value from acting as a
    /// finalisation capability.
    fn finish_delalloc_reservation(&mut self, reservation: &mut DelallocReservation) -> Result<()> {
        let reservation_id = reservation.id;
        if !reservation.active {
            return_error!(
                ErrCode::EINVAL,
                "Delayed-allocation lease was already released or finalised"
            );
        }
        if reservation_id.mount_generation != self.mount_generation {
            return_error!(
                ErrCode::EINVAL,
                "Delayed-allocation reservation belongs to another filesystem instance"
            );
        }
        let claim = self.delalloc_claims.get(&reservation_id).ok_or_else(|| {
            format_error!(ErrCode::EINVAL, "Unknown delayed-allocation reservation")
        })?;
        if claim.data_blocks != 0 || claim.metadata_blocks != 0 {
            return_error!(
                ErrCode::EIO,
                "Delayed-allocation reservation finished with unconsumed blocks"
            );
        }
        if claim.inflight_consumptions != 0 {
            return_error!(
                ErrCode::EIO,
                "Delayed-allocation reservation has an unfinished allocation"
            );
        }
        let removed = self.delalloc_claims.remove(&reservation_id);
        debug_assert!(removed.is_some());
        reservation.deactivate();
        Ok(())
    }

    /// Atomically commit the data debit and optional metadata debit belonging
    /// to one append mapper, then return conservatively reserved but unused
    /// metadata capacity.  A mapper may reserve a split credit before it can
    /// know whether the final physical run merges into the previous extent;
    /// retaining that unused capacity would turn a valid writeback into a
    /// permanent space leak.  Conversely, releasing it before the journal
    /// commit would let an eager allocator steal capacity still needed by the
    /// transaction.  This helper performs the only valid transition after a
    /// successful/uncertain publication point.
    fn finalize_append_block_reservation(
        &mut self,
        reservation: &mut DelallocReservation,
        data: &mut DelallocConsumption,
        metadata: Option<&mut DelallocConsumption>,
    ) -> Result<()> {
        let mut metadata = metadata;
        let result = (|| {
            if !reservation.active || reservation.id.mount_generation != self.mount_generation {
                return_error!(ErrCode::EINVAL, "Invalid delayed-allocation append lease");
            }

            let mut consumption_count = 1u64;
            let data_record = *self
                .delalloc_consumptions
                .get(&data.serial)
                .ok_or_else(|| Ext4Error::new(ErrCode::EINVAL))?;
            if !data.unresolved
                || data.mount_generation != self.mount_generation
                || data_record.reservation != reservation.id
                || data_record.use_kind != DelallocReservationUse::Data
                || data_record.blocks != reservation.data_blocks
            {
                return_error!(ErrCode::EINVAL, "Invalid delayed data debit finalisation");
            }

            let metadata_record = if let Some(metadata) = metadata.as_deref() {
                consumption_count = consumption_count
                    .checked_add(1)
                    .ok_or_else(|| Ext4Error::new(ErrCode::ERANGE))?;
                let record = *self
                    .delalloc_consumptions
                    .get(&metadata.serial)
                    .ok_or_else(|| Ext4Error::new(ErrCode::EINVAL))?;
                if !metadata.unresolved
                    || metadata.mount_generation != self.mount_generation
                    || metadata.serial == data.serial
                    || record.reservation != reservation.id
                    || record.use_kind != DelallocReservationUse::Metadata
                    || record.blocks != 1
                {
                    return_error!(
                        ErrCode::EINVAL,
                        "Invalid delayed metadata debit finalisation"
                    );
                }
                Some(record)
            } else {
                None
            };

            let claim = self.delalloc_claims.get(&reservation.id).ok_or_else(|| {
                format_error!(ErrCode::EIO, "Delayed-allocation append claim disappeared")
            })?;
            if claim.inflight_consumptions != consumption_count || claim.data_blocks != 0 {
                return_error!(
                    ErrCode::EIO,
                    "Delayed-allocation append claim is inconsistent"
                );
            }

            // `consume_delalloc()` already removed each debit from the global
            // reserved counters.  Only the still-unconsumed metadata tail remains
            // accounted here and must be returned as the claim disappears.
            let released_metadata = claim.metadata_blocks;
            let new_reserved_metadata = self
                .reserved_metadata_blocks
                .checked_sub(released_metadata)
                .ok_or_else(|| {
                    format_error!(
                        ErrCode::EIO,
                        "Delayed-allocation metadata reservation accounting underflowed"
                    )
                })?;
            let removed_data = self.delalloc_consumptions.remove(&data.serial);
            debug_assert_eq!(removed_data, Some(data_record));
            data.resolve();
            if let Some(metadata) = metadata.as_mut() {
                let metadata = &mut **metadata;
                let record = metadata_record.expect("metadata record was validated");
                let removed_metadata = self.delalloc_consumptions.remove(&metadata.serial);
                debug_assert_eq!(removed_metadata, Some(record));
                metadata.resolve();
            }
            let removed_claim = self.delalloc_claims.remove(&reservation.id);
            debug_assert!(removed_claim.is_some());
            self.reserved_metadata_blocks = new_reserved_metadata;
            reservation.deactivate();
            Ok(())
        })();
        if result.is_err() {
            // Publication has already succeeded or become uncertain when this
            // helper is used.  Fail-stop recovery owns the inconsistent
            // in-memory ledger; resolve the linear tokens so returning EIO
            // cannot panic during unwinding.
            data.resolve();
            if let Some(metadata) = metadata.as_mut() {
                let metadata = &mut **metadata;
                metadata.resolve();
            }
            reservation.deactivate();
        }
        result
    }
}

fn transaction_range_probe_limit(block_group_count: u32, require_preferred: bool) -> u32 {
    // An exact-adjacency request can only succeed in its preferred group.  A
    // failed probe is therefore a fast-path miss, not a reason to scan other
    // groups while holding the transaction gate.
    let limit = if require_preferred {
        1
    } else {
        TRANSACTION_RANGE_MAX_GROUP_PROBES
    };
    core::cmp::min(block_group_count, limit)
}

fn extent_tail_batch_limit(
    first_data_block: PBlockId,
    blocks_per_group: PBlockId,
    start: PBlockId,
    count: u32,
) -> Option<u32> {
    if blocks_per_group == 0 || count == 0 || start < first_data_block {
        return None;
    }
    let last = start.checked_add(count as PBlockId)?.checked_sub(1)?;
    let in_last_group = (last - first_data_block) % blocks_per_group + 1;
    Some(core::cmp::min(count, in_last_group as u32))
}

fn linked_orphan_tail_remove_limit(
    keep_blocks: u64,
    tail_start: u32,
    tail_blocks: u32,
    group_limit: u32,
) -> Option<u32> {
    let tail_end = tail_start as u64 + tail_blocks as u64;
    if tail_end <= keep_blocks {
        return None;
    }
    let beyond_eof = tail_end - core::cmp::max(keep_blocks, tail_start as u64);
    Some(core::cmp::min(
        group_limit,
        core::cmp::min(beyond_eof, u32::MAX as u64) as u32,
    ))
}

impl Ext4 {
    /// Apply one clean delayed-allocation capacity mutation.
    ///
    /// The final poison check and the ledger update share the short
    /// `alloc_lock -> poisoned` critical section. `poison()` takes only the
    /// latter mutex, so the winner is unambiguous: a mutation which obtains it
    /// first finishes before fail-stop; a fail-stop which obtains it first
    /// rejects the mutation and its owner must abandon rather than restore
    /// capacity. Never call this helper across I/O or while holding the
    /// poisoned mutex in the opposite order.
    fn mutate_clean_delalloc_ledger<T>(
        &self,
        mutation: impl FnOnce(&mut AllocationState) -> Result<T>,
    ) -> Result<T> {
        let mut allocation = self.alloc_lock.lock();
        let poisoned = self.poisoned.lock();
        if let Some(code) = *poisoned {
            return Err(Ext4Error::new(code));
        }
        mutation(&mut allocation)
    }

    /// Host-only injection point for the fail-stop/ledger linearization test.
    /// The hook runs after both short locks are acquired and before the claim
    /// is inserted, so the test can deterministically place fail-stop on the
    /// losing side without adding a production scheduling hook.
    #[cfg(test)]
    pub(super) fn test_reserve_clean_delalloc_lease_with_hook(
        &self,
        before_commit: impl FnOnce(),
    ) -> Result<DelallocLease> {
        self.mutate_clean_delalloc_ledger(|allocation| {
            before_commit();
            allocation.reserve_delalloc(8, 1, 0)
        })
    }

    #[cfg(test)]
    pub(super) fn test_release_clean_delalloc_lease(
        &self,
        lease: &mut DelallocLease,
        before_commit: impl FnOnce(),
    ) -> Result<()> {
        self.mutate_clean_delalloc_ledger(|allocation| {
            before_commit();
            allocation.release_delalloc(lease)
        })
    }

    /// Reserve the blocks a future delayed-allocation write will need before
    /// the write is reported successful to its caller.
    ///
    /// This changes only in-memory admission accounting: it does not allocate
    /// a bitmap bit, write an extent, or alter inode size.  The subsequent
    /// PageCache-to-ext4 writeback token owns explicit release or consumption.
    pub fn reserve_delalloc_lease(
        &self,
        data_blocks: u64,
        metadata_blocks: u64,
    ) -> Result<DelallocLease> {
        self.ensure_mutable()?;
        // Delayed allocation has no publication/recovery protocol on a
        // no-journal mount.  Do not admit a claim which the current eager
        // path cannot safely consume or recover after power loss.
        if !self.uses_journal() {
            return_error!(
                ErrCode::ENOTSUP,
                "Delayed allocation requires a journal-backed mount"
            );
        }
        // A journal transaction keeps its new free-block counter in a
        // transaction-private image until commit.  Taking the non-blocking
        // direct gate first makes that transaction and this cached-counter
        // snapshot mutually exclusive; otherwise a reservation could observe
        // the old cached count between transaction staging and commit.
        //
        // This guard covers only global accounting and is dropped before the
        // caller can enter PageCache or per-inode state.  It therefore cannot
        // become a writeback lifetime token or introduce a reverse lock edge.
        let _mutation_guard = self.lock_direct_metadata_mutation()?;
        let free_blocks = self.read_super_block_cached().free_blocks_count();
        self.mutate_clean_delalloc_ledger(|allocation| {
            allocation.reserve_delalloc(free_blocks, data_blocks, metadata_blocks)
        })
    }

    /// Reserve while the caller already owns the direct metadata exclusion
    /// domain and the target inode shard.  This is used by the append
    /// admission path so shape validation and the ledger claim share one
    /// no-writer window; reacquiring the direct gate here would self-conflict.
    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    pub(super) fn reserve_delalloc_lease_in_direct_mutation_domain(
        &self,
        data_blocks: u64,
        metadata_blocks: u64,
    ) -> Result<DelallocLease> {
        self.ensure_mutable()?;
        if !self.uses_journal() {
            return_error!(
                ErrCode::ENOTSUP,
                "Delayed allocation requires a journal-backed mount"
            );
        }
        let free_blocks = self.read_super_block_cached().free_blocks_count();
        self.mutate_clean_delalloc_ledger(|allocation| {
            allocation.reserve_delalloc(free_blocks, data_blocks, metadata_blocks)
        })
    }

    /// Release a set of complete, still-unmaterialised delayed-allocation
    /// leases.  The filesystem canonicalises its opaque identities internally;
    /// callers need not and cannot sort by an exposed lease id. The provided
    /// reference slice may be reordered, but the leases themselves retain
    /// their ownership and are only deactivated after full validation.
    ///
    /// The operation is all-or-nothing.  On error every lease remains live
    /// and belongs back in its queue entry; on success all leases are made
    /// inactive before this method returns.  This is a low-level primitive for
    /// a consuming VFS bridge, not a permission to release a partial mapping.
    pub fn release_delalloc_lease_batch(&self, leases: &mut [&mut DelallocLease]) -> Result<()> {
        self.ensure_mutable()?;
        let _mutation_guard = self.lock_direct_metadata_mutation()?;
        self.mutate_clean_delalloc_ledger(|allocation| allocation.release_delalloc_batch(leases))
    }

    /// Terminalise a lease whose mount has already fail-stopped.
    ///
    /// This deliberately does not mutate the ledger or restore capacity: a
    /// poisoned mount must never allocate again.  It is the only valid owner
    /// teardown path when a different operation poisons the mount before a
    /// queued delayed-allocation lease reaches a mapper.  The mount-generation
    /// check prevents using this as a cross-mount capability destructor.
    pub fn abandon_delalloc_lease_after_fail_stop(&self, lease: &mut DelallocLease) -> Result<()> {
        let allocation = self.alloc_lock.lock();
        let poisoned = self.poisoned.lock();
        if !lease.active || poisoned.is_none() {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        if lease.id.mount_generation != allocation.mount_generation {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        lease.deactivate();
        Ok(())
    }

    /// Compatibility name retained for host fault-injection callers.  The
    /// production API above has the same fail-stop-only semantics.
    #[cfg(any(test, feature = "test-api"))]
    pub fn test_abandon_delalloc_lease_after_fail_stop(
        &self,
        lease: &mut DelallocLease,
    ) -> Result<()> {
        self.abandon_delalloc_lease_after_fail_stop(lease)
    }

    /// Release a lease after its owning journal transaction has been aborted
    /// but before the caller leaves the transactional metadata exclusion
    /// domain. Taking the normal direct-writer gate here would self-conflict
    /// with that domain; the caller instead proves that no direct writer can
    /// observe the ledger transition. This stays crate-private so normal VFS
    /// cancellation continues to use the checked public API above.
    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    pub(super) fn release_delalloc_lease_after_transaction_abort(
        &self,
        lease: &mut DelallocLease,
    ) -> Result<()> {
        self.mutate_clean_delalloc_ledger(|allocation| allocation.release_delalloc(lease))
    }

    /// Immutable provenance for every delayed-allocation capability issued by
    /// this mounted `Ext4` instance.  The atomic allocation lock is also the
    /// authority for this generation, so a receipt can reject a same-numbered
    /// inode from a different mount before it reaches data I/O.
    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    pub(super) fn delalloc_mount_generation(&self) -> u64 {
        self.alloc_lock.lock().mount_generation
    }

    fn restore_inode_allocation_state(
        &self,
        bitmap_block: &Block,
        bg: &BlockGroupRef,
        sb: &SuperBlock,
    ) -> Result<()> {
        self.write_block(bitmap_block)?;
        self.write_block_group_with_csum(&mut BlockGroupRef::new(bg.id, bg.desc))?;
        self.write_super_block(sb)
    }
    fn restore_block_allocation_state(
        &self,
        bitmap_block: &Block,
        bg: &BlockGroupRef,
        sb: &SuperBlock,
    ) -> Result<()> {
        self.write_block(bitmap_block)?;
        self.write_block_group_with_csum(&mut BlockGroupRef::new(bg.id, bg.desc))?;
        self.write_super_block(sb)
    }

    fn block_group_first_block(sb: &SuperBlock, bgid: BlockGroupId) -> PBlockId {
        sb.first_data_block() as PBlockId + bgid as PBlockId * sb.blocks_per_group() as PBlockId
    }

    fn block_group_block_count(sb: &SuperBlock, bgid: BlockGroupId) -> usize {
        let first = Self::block_group_first_block(sb, bgid);
        let total = sb.block_count();
        if first >= total {
            return 0;
        }
        core::cmp::min(sb.blocks_per_group() as u64, total - first) as usize
    }

    /// Stage one group-local contiguous data allocation without publishing
    /// any cache or disk metadata.  The caller owns the filesystem-wide
    /// transactional metadata gate, so no direct allocator can race the
    /// transaction-private bitmap snapshot while zero I/O is in progress.
    fn transaction_alloc_range_with_class(
        &self,
        transaction: &mut super::journal_transaction::Transaction<'_>,
        request: TransactionRangeAllocationRequest,
    ) -> Result<(RangeAllocation, Option<DelallocConsumption>)> {
        let TransactionRangeAllocationRequest {
            inode_id,
            preferred_first,
            require_preferred,
            count,
            class,
            reservation_use,
            probe_limit,
        } = request;
        // The transaction writer gate serializes metadata publication, while
        // this lock also protects reservation accounting.  Keep this order
        // (gate -> inode shard -> transaction -> alloc) for all future
        // delayed-allocation materialisation paths.
        let mut allocation = self.alloc_lock.lock();
        if count == 0 {
            return_error!(ErrCode::EINVAL, "Cannot allocate an empty block range");
        }
        let mut sb = self.transaction_read_super_block(transaction)?;
        if matches!(class, AllocationClass::Unreserved) {
            allocation.ensure_unreserved_capacity(sb.free_blocks_count(), count as u64)?;
        }
        self.prepare_stats.record_superblock_io();
        let bg_count = sb.block_group_count();
        let preferred_inode_group = ((inode_id - 1) / sb.inodes_per_group()) as BlockGroupId;
        let preferred_group = preferred_first
            .filter(|block| *block >= sb.first_data_block() as PBlockId)
            .map(|block| {
                ((block - sb.first_data_block() as PBlockId) / sb.blocks_per_group() as PBlockId)
                    as BlockGroupId
            })
            .filter(|group| *group < bg_count)
            .unwrap_or(preferred_inode_group);
        let count_usize = count as usize;

        // This is a speculative fast path.  Bound its metadata I/O under the
        // transaction writer gate; on a miss the caller aborts the
        // transaction and uses the legacy allocator.  Exact-adjacency probes
        // need only inspect the preferred group.
        let probe_limit = core::cmp::min(
            bg_count,
            core::cmp::max(probe_limit, u32::from(require_preferred)),
        );
        for offset in 0..probe_limit {
            let bgid = ((preferred_group as u64 + offset as u64) % bg_count as u64) as BlockGroupId;
            let blocks_in_group = Self::block_group_block_count(&sb, bgid);
            if blocks_in_group < count_usize {
                continue;
            }
            let mut bg = self.transaction_read_block_group(transaction, bgid)?;
            self.prepare_stats.record_gdt_io();
            if bg.desc.get_free_blocks_count() < count as u64 {
                continue;
            }
            let bitmap_home = bg.desc.block_bitmap_block();
            let bitmap_block = transaction.read(self.block_device.as_ref(), bitmap_home)?;
            self.prepare_stats.record_bitmap_io();
            let checksum_bytes = (sb.clusters_per_group() as usize) / 8;
            if sb.has_read_only_compatible_feature(SuperBlock::FEATURE_RO_COMPAT_METADATA_CSUM) {
                if !bg.verify_checksum(sb.metadata_checksum_seed()) {
                    return_error!(ErrCode::EIO, "Corrupt block-group descriptor checksum");
                }
                if !bg.desc.verify_block_bitmap_csum(
                    sb.metadata_checksum_seed(),
                    &*bitmap_block,
                    checksum_bytes,
                ) {
                    return_error!(ErrCode::EIO, "Corrupt block bitmap checksum");
                }
            }

            let group_first = Self::block_group_first_block(&sb, bgid);
            let exact_hint = preferred_first
                .filter(|_| offset == 0)
                .and_then(|block| block.checked_sub(group_first))
                .and_then(|bit| usize::try_from(bit).ok())
                .filter(|bit| {
                    bit.checked_add(count_usize)
                        .is_some_and(|end| end <= blocks_in_group)
                        && (*bit..*bit + count_usize)
                            .all(|index| bitmap_block[index / 8] & (1 << (index % 8)) == 0)
                });
            let bit = exact_hint.or_else(|| {
                (!require_preferred)
                    .then(|| {
                        Bitmap::first_clear_run_in(
                            &*bitmap_block,
                            blocks_in_group,
                            0,
                            blocks_in_group,
                            count_usize,
                        )
                    })
                    .flatten()
            });
            let Some(bit) = bit else { continue };
            let first = group_first
                .checked_add(bit as PBlockId)
                .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
            self.validate_data_blocks(first, count as u64)?;
            let new_bg_free = bg
                .desc
                .get_free_blocks_count()
                .checked_sub(count as u64)
                .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
            let new_sb_free = sb
                .free_blocks_count()
                .checked_sub(count as u64)
                .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;

            {
                let image = self.transaction_block_for_update(transaction, bitmap_home)?;
                self.prepare_stats.record_bitmap_io();
                let mut bitmap = Bitmap::new(image, blocks_in_group);
                if (bit..bit + count_usize).any(|index| !bitmap.is_bit_clear(index)) {
                    return_error!(ErrCode::EIO, "Range allocation changed during planning");
                }
                for index in bit..bit + count_usize {
                    bitmap.set_bit(index);
                }
                if !bg.desc.update_block_bitmap_csum(
                    sb.metadata_checksum_seed(),
                    image,
                    checksum_bytes,
                ) {
                    return_error!(ErrCode::EIO, "Invalid block bitmap checksum length");
                }
            }
            bg.desc.set_free_blocks_count(new_bg_free);
            self.transaction_stage_block_group_with_csum(transaction, &mut bg)?;
            self.prepare_stats.record_gdt_io();
            sb.set_free_blocks_count(new_sb_free);
            self.transaction_stage_super_block(transaction, &sb)?;
            self.prepare_stats.record_superblock_io();
            let (gdt_home, _) = self.block_group_disk_pos(bgid)?;
            let consumption = allocation.consume_delalloc(class, reservation_use, count as u64)?;
            return Ok((
                RangeAllocation {
                    first,
                    allocation_homes: [bitmap_home, gdt_home, 0],
                },
                consumption,
            ));
        }
        return_error!(ErrCode::ENOSPC, "No contiguous direct range available");
    }

    /// Allocate an ordinary eager range.  The bounded probe policy remains a
    /// fast-path performance contract for foreground eager writes; delayed
    /// writeback uses the separate reserved helper below because a successful
    /// reservation must not later fail merely due to this speculative limit.
    pub(super) fn transaction_alloc_range(
        &self,
        transaction: &mut super::journal_transaction::Transaction<'_>,
        inode_id: InodeId,
        preferred_first: Option<PBlockId>,
        require_preferred: bool,
        count: u32,
    ) -> Result<RangeAllocation> {
        let (allocation, consumption) = self.transaction_alloc_range_with_class(
            transaction,
            TransactionRangeAllocationRequest {
                inode_id,
                preferred_first,
                require_preferred,
                count,
                class: AllocationClass::Unreserved,
                reservation_use: DelallocReservationUse::Data,
                probe_limit: transaction_range_probe_limit(
                    self.read_super_block_cached().block_group_count(),
                    require_preferred,
                ),
            },
        )?;
        debug_assert!(consumption.is_none());
        Ok(allocation)
    }

    /// Materialise an already reserved delayed-allocation data range.
    ///
    /// A lease represents capacity which was promised before the user write
    /// became visible.  Unlike the eager fast path, search every block group:
    /// the reservation protects aggregate free space, so a bounded local
    /// probe is not allowed to manufacture a late `ENOSPC`.  The caller must
    /// still restrict the range to a shape with a feasible contiguous run;
    /// phase 3b currently uses exactly one block.
    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    pub(super) fn transaction_alloc_delalloc_range(
        &self,
        transaction: &mut super::journal_transaction::Transaction<'_>,
        inode_id: InodeId,
        preferred_first: Option<PBlockId>,
        require_preferred: bool,
        count: u32,
        lease: &DelallocLease,
    ) -> Result<(RangeAllocation, DelallocConsumption)> {
        if !lease.active {
            return_error!(ErrCode::EINVAL, "Delayed-allocation lease is inactive");
        }
        let groups = self
            .transaction_read_super_block(transaction)?
            .block_group_count();
        let (allocation, consumption) = self.transaction_alloc_range_with_class(
            transaction,
            TransactionRangeAllocationRequest {
                inode_id,
                preferred_first,
                require_preferred,
                count,
                class: AllocationClass::Delalloc(lease.id),
                reservation_use: DelallocReservationUse::Data,
                probe_limit: groups,
            },
        )?;
        let consumption = consumption.expect("delalloc allocation must create a debit");
        Ok((allocation, consumption))
    }

    /// Materialise one metadata block from an already reserved delayed
    /// allocation claim.  Extent-root/leaf growth is accounted separately
    /// from data blocks so a foreground write cannot succeed merely because
    /// an unrelated data reservation happens to be large enough.
    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    pub(super) fn transaction_alloc_delalloc_metadata_block(
        &self,
        transaction: &mut super::journal_transaction::Transaction<'_>,
        inode_id: InodeId,
        preferred_first: Option<PBlockId>,
        lease: &DelallocLease,
    ) -> Result<(RangeAllocation, DelallocConsumption)> {
        if !lease.active {
            return_error!(ErrCode::EINVAL, "Delayed-allocation lease is inactive");
        }
        let groups = self
            .transaction_read_super_block(transaction)?
            .block_group_count();
        let (allocation, consumption) = self.transaction_alloc_range_with_class(
            transaction,
            TransactionRangeAllocationRequest {
                inode_id,
                preferred_first,
                require_preferred: false,
                count: 1,
                class: AllocationClass::Delalloc(lease.id),
                reservation_use: DelallocReservationUse::Metadata,
                probe_limit: groups,
            },
        )?;
        let consumption = consumption.expect("delalloc metadata allocation must create a debit");
        Ok((allocation, consumption))
    }

    /// Undo a staged delayed data allocation before its journal transaction
    /// reaches a publication point.
    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    pub(super) fn rollback_delalloc_allocation(
        &self,
        consumption: DelallocConsumption,
    ) -> Result<()> {
        let mut allocation = self.alloc_lock.lock();
        allocation.rollback_delalloc_consumption(consumption)
    }

    /// Commit a delayed data debit after the mapping transaction has either
    /// completed or entered an uncertain journal state.  The latter poisons
    /// the filesystem at the caller, so no live claim may be left behind.
    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    pub(super) fn commit_delalloc_allocation(
        &self,
        consumption: DelallocConsumption,
        lease: &mut DelallocLease,
    ) -> Result<()> {
        let mut allocation = self.alloc_lock.lock();
        allocation.commit_delalloc_consumption(consumption)?;
        allocation.finish_delalloc_reservation(lease)
    }

    /// Complete one append mapper after its journal transaction has crossed
    /// the publication point.  Any unused conservative split credit is
    /// released only here, never while the transaction can still need it.
    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    pub(super) fn finalize_delalloc_append_block(
        &self,
        lease: &mut DelallocLease,
        data: &mut DelallocConsumption,
        metadata: Option<&mut DelallocConsumption>,
    ) -> Result<()> {
        let mut allocation = self.alloc_lock.lock();
        allocation.finalize_append_block_reservation(lease, data, metadata)
    }

    pub(super) fn finalize_delalloc_append_block_with_pool(
        &self,
        data_lease: &mut DelallocLease,
        data: &mut DelallocConsumption,
        pool_entry: Option<(&mut DelallocLease, &mut DelallocConsumption)>,
    ) -> Result<()> {
        let mut allocation = self.alloc_lock.lock();
        let data = core::mem::replace(
            data,
            DelallocConsumption {
                mount_generation: 0,
                serial: 0,
                unresolved: false,
            },
        );
        allocation.commit_delalloc_consumption(data)?;
        allocation.finish_delalloc_reservation(data_lease)?;
        if let Some((pool_lease, metadata)) = pool_entry {
            let metadata = core::mem::replace(
                metadata,
                DelallocConsumption {
                    mount_generation: 0,
                    serial: 0,
                    unresolved: false,
                },
            );
            allocation.commit_delalloc_consumption(metadata)?;
            allocation.finish_delalloc_reservation(pool_lease)?;
        }
        Ok(())
    }

    /// Stage the release of one contiguous physical-block range.
    ///
    /// This changes allocation metadata only.  In particular, freed data is
    /// not zeroed: after commit the blocks no longer belong to this inode and
    /// clearing them would be both unnecessary I/O and a race with reuse.
    pub(super) fn transaction_dealloc_block_range(
        &self,
        transaction: &mut super::journal_transaction::Transaction<'_>,
        first: PBlockId,
        count: u32,
    ) -> Result<()> {
        let _alloc_guard = self.alloc_lock.lock();
        if count == 0 {
            return_error!(ErrCode::EINVAL, "Cannot free an empty block range");
        }

        let mut sb = self.transaction_read_super_block(transaction)?;
        let last_exclusive = first
            .checked_add(count as PBlockId)
            .ok_or_else(|| format_error!(ErrCode::EINVAL, "Block range overflows"))?;
        if first < sb.first_data_block() as PBlockId
            || first >= sb.block_count()
            || last_exclusive > sb.block_count()
        {
            return_error!(
                ErrCode::EINVAL,
                "Invalid block range {}..{}",
                first,
                last_exclusive
            );
        }
        let blocks_per_group = sb.blocks_per_group() as PBlockId;
        let data_first = sb.first_data_block() as PBlockId;
        let bgid = ((first - data_first) / blocks_per_group) as BlockGroupId;
        if ((last_exclusive - 1 - data_first) / blocks_per_group) as BlockGroupId != bgid {
            return_error!(ErrCode::EINVAL, "Block range crosses a block group");
        }
        let group_first = Self::block_group_first_block(&sb, bgid);
        let bit = (first - group_first) as usize;
        let count = count as usize;
        let blocks_in_group = Self::block_group_block_count(&sb, bgid);
        if bit
            .checked_add(count)
            .is_none_or(|end| end > blocks_in_group)
        {
            return_error!(ErrCode::EINVAL, "Block range exceeds block group");
        }
        self.validate_data_blocks(first, count as u64)?;

        let mut bg = self.transaction_read_block_group(transaction, bgid)?;
        let bitmap_block_id = bg.desc.block_bitmap_block();
        let metadata_csum =
            sb.has_read_only_compatible_feature(SuperBlock::FEATURE_RO_COMPAT_METADATA_CSUM);
        let checksum_bytes = (sb.clusters_per_group() as usize) / 8;
        if metadata_csum {
            if !bg.verify_checksum(sb.metadata_checksum_seed()) {
                return_error!(ErrCode::EIO, "Corrupt block-group descriptor checksum");
            }
            let bitmap_image = transaction.read(self.block_device.as_ref(), bitmap_block_id)?;
            if !bg.desc.verify_block_bitmap_csum(
                sb.metadata_checksum_seed(),
                &*bitmap_image,
                checksum_bytes,
            ) {
                return_error!(ErrCode::EIO, "Corrupt block bitmap checksum");
            }
        }
        // Validate all accounting before mutating the transaction image.  A
        // corrupt counter must not leave a half-applied bitmap update behind.
        let new_bg_free = bg
            .desc
            .get_free_blocks_count()
            .checked_add(count as u64)
            .filter(|value| *value <= blocks_in_group as u64)
            .ok_or_else(|| format_error!(ErrCode::EINVAL, "Invalid block-group free count"))?;
        let new_sb_free = sb
            .free_blocks_count()
            .checked_add(count as u64)
            .filter(|value| *value <= sb.block_count())
            .ok_or_else(|| format_error!(ErrCode::EINVAL, "Invalid filesystem free count"))?;
        {
            let image = self.transaction_block_for_update(transaction, bitmap_block_id)?;
            let mut bitmap = Bitmap::new(image, blocks_in_group);
            if (bit..bit + count).any(|index| bitmap.is_bit_clear(index)) {
                return_error!(ErrCode::EINVAL, "Block range contains a free block");
            }
            for index in bit..bit + count {
                bitmap.clear_bit(index);
            }
            if metadata_csum
                && !bg.desc.update_block_bitmap_csum(
                    sb.metadata_checksum_seed(),
                    image,
                    checksum_bytes,
                )
            {
                return_error!(ErrCode::EIO, "Invalid block bitmap checksum length");
            }
        }

        bg.desc.set_free_blocks_count(new_bg_free);
        self.transaction_stage_block_group_with_csum(transaction, &mut bg)?;
        sb.set_free_blocks_count(new_sb_free);
        self.transaction_stage_super_block(transaction, &sb)
    }

    /// Stage final inode-number release.  `itable_unused` describes the
    /// never-initialized tail of the inode table, not reusable inode slots, so
    /// freeing an inode must leave it unchanged (as Linux ext4 does).
    pub(super) fn transaction_dealloc_inode(
        &self,
        transaction: &mut super::journal_transaction::Transaction<'_>,
        inode_id: InodeId,
        is_dir: bool,
    ) -> Result<()> {
        let _alloc_guard = self.alloc_lock.lock();
        let mut sb = self.transaction_read_super_block(transaction)?;
        if inode_id == 0 || inode_id > sb.inode_count() {
            return_error!(ErrCode::EINVAL, "Invalid inode {}", inode_id);
        }
        let inodes_per_group = sb.inodes_per_group();
        let bgid = ((inode_id - 1) / inodes_per_group) as BlockGroupId;
        let idx = ((inode_id - 1) % inodes_per_group) as usize;
        let inode_count = sb.inode_count_in_group(bgid) as usize;
        if idx >= inode_count {
            return_error!(ErrCode::EINVAL, "Invalid inode index {}", idx);
        }

        let mut bg = self.transaction_read_block_group(transaction, bgid)?;
        let bitmap_block_id = bg.desc.inode_bitmap_block();
        let metadata_csum =
            sb.has_read_only_compatible_feature(SuperBlock::FEATURE_RO_COMPAT_METADATA_CSUM);
        let checksum_bytes = (sb.inodes_per_group() as usize) / 8;
        if metadata_csum {
            if !bg.verify_checksum(sb.metadata_checksum_seed()) {
                return_error!(ErrCode::EIO, "Corrupt block-group descriptor checksum");
            }
            let bitmap_image = transaction.read(self.block_device.as_ref(), bitmap_block_id)?;
            if !bg.desc.verify_inode_bitmap_csum(
                sb.metadata_checksum_seed(),
                &*bitmap_image,
                checksum_bytes,
            ) {
                return_error!(ErrCode::EIO, "Corrupt inode bitmap checksum");
            }
        }
        let new_bg_free = bg
            .desc
            .free_inodes_count()
            .checked_add(1)
            .filter(|value| *value <= inode_count as u32)
            .ok_or_else(|| format_error!(ErrCode::EINVAL, "Invalid block-group inode count"))?;
        let new_sb_free = sb
            .free_inodes_count()
            .checked_add(1)
            .filter(|value| *value <= sb.inode_count())
            .ok_or_else(|| format_error!(ErrCode::EINVAL, "Invalid filesystem inode count"))?;
        let new_used_dirs =
            if is_dir {
                Some(bg.desc.used_dirs_count().checked_sub(1).ok_or_else(|| {
                    format_error!(ErrCode::EINVAL, "Invalid used-directory count")
                })?)
            } else {
                None
            };
        {
            let image = self.transaction_block_for_update(transaction, bitmap_block_id)?;
            let mut bitmap = Bitmap::new(image, inode_count);
            if bitmap.is_bit_clear(idx) {
                return_error!(ErrCode::EINVAL, "Inode {} is already free", inode_id);
            }
            bitmap.clear_bit(idx);
            if metadata_csum
                && !bg.desc.update_inode_bitmap_csum(
                    sb.metadata_checksum_seed(),
                    image,
                    checksum_bytes,
                )
            {
                return_error!(ErrCode::EIO, "Invalid inode bitmap checksum length");
            }
        }

        bg.desc.set_free_inodes_count(new_bg_free);
        if let Some(used) = new_used_dirs {
            bg.desc.set_used_dirs_count(used);
        }
        self.transaction_stage_block_group_with_csum(transaction, &mut bg)?;
        sb.set_free_inodes_count(new_sb_free);
        self.transaction_stage_super_block(transaction, &sb)
    }

    /// Create a new inode with its final owner, returning the inode and its number.
    #[inline(never)]
    pub(super) fn create_inode_with_owner(
        &self,
        mode: InodeMode,
        uid: u32,
        gid: u32,
    ) -> Result<InodeRef> {
        self.ensure_mutable()?;
        // Allocate an inode
        let is_dir = mode.file_type() == FileType::Directory;
        let id = self.alloc_inode(is_dir)?;

        let initialized = (|| {
            let generation = self.next_inode_generation(id)?;
            let mut inode = Box::new(Inode::default());
            inode.set_generation(generation);
            inode.set_mode(mode);
            inode.set_uid(uid);
            inode.set_gid(gid);
            inode.extent_init();
            let mut inode_ref = InodeRef::new(id, inode);
            self.write_inode_with_csum(&mut inode_ref)?;
            Ok(inode_ref)
        })();
        let inode_ref = match initialized {
            Ok(inode_ref) => inode_ref,
            Err(error) => {
                if self.rollback_new_inode(id, is_dir).is_err() {
                    self.poison(ErrCode::EIO);
                }
                return Err(error);
            }
        };

        trace!("Alloc inode {} ok", inode_ref.id);
        Ok(inode_ref)
    }

    /// Create a device inode (character or block device).
    ///
    /// Unlike `create_inode()`, this function:
    /// - Does NOT initialize the extent tree
    /// - Stores the device number in i_block[0..1] (Linux ext4 standard)
    #[inline(never)]
    pub(super) fn create_device_inode(
        &self,
        mode: InodeMode,
        major: u32,
        minor: u32,
        uid: u32,
        gid: u32,
    ) -> Result<InodeRef> {
        self.ensure_mutable()?;
        // Device nodes are never directories
        let id = self.alloc_inode(false)?;

        let initialized = (|| {
            let generation = self.next_inode_generation(id)?;
            let mut inode = Box::new(Inode::default());
            inode.set_generation(generation);
            inode.set_mode(mode);
            inode.set_uid(uid);
            inode.set_gid(gid);
            inode.set_device(major, minor);
            let mut inode_ref = InodeRef::new(id, inode);
            self.write_inode_with_csum(&mut inode_ref)?;
            Ok(inode_ref)
        })();
        let inode_ref = match initialized {
            Ok(inode_ref) => inode_ref,
            Err(error) => {
                if self.rollback_new_inode(id, false).is_err() {
                    self.poison(ErrCode::EIO);
                }
                return Err(error);
            }
        };

        trace!(
            "Alloc device inode {} ({}:{}) ok",
            inode_ref.id,
            major,
            minor
        );
        Ok(inode_ref)
    }

    /// Create(initialize) the root inode of the file system
    #[inline(never)]
    pub(super) fn create_root_inode(&self) -> Result<InodeRef> {
        let mut inode = Box::new(Inode::default());
        inode.set_mode(InodeMode::from_type_and_perm(
            FileType::Directory,
            InodeMode::from_bits_retain(0o755),
        ));
        inode.extent_init();

        let mut root = InodeRef::new(EXT4_ROOT_INO, inode);
        let root_self = root.clone();

        // Add `.` and `..` entries
        self.dir_add_entry(&mut root, &root_self, ".")?;
        self.dir_add_entry(&mut root, &root_self, "..")?;
        root.inode.set_link_count(2);

        self.write_inode_with_csum(&mut root)?;
        Ok(root)
    }

    /// Roll back an allocated inode that has not been published in a directory.
    ///
    /// The validated in-memory extent tree is authoritative here: an inode
    /// write may fail after the tree was updated but before `i_blocks` was
    /// recomputed, so the transient on-disk count is not a rollback precondition.
    pub(super) fn free_inode(&self, inode: &mut InodeRef) -> Result<()> {
        let inode_id = inode.id;
        if inode.inode.uses_extents() {
            let data_blocks = self.extent_all_data_blocks(inode)?;
            let tree_blocks = self.extent_all_tree_blocks(inode)?;
            for pblock in data_blocks {
                self.dealloc_block(inode, pblock)?;
            }
            for pblock in tree_blocks {
                self.dealloc_block(inode, pblock)?;
            }
            // dealloc_block updates allocation metadata, not the in-memory
            // inode that is about to be released.
            inode.inode.set_fs_block_count(0);
        } else if inode.inode.fs_block_count() != 0 {
            return_error!(ErrCode::EIO, "Inline inode owns unexplained blocks");
        }
        // Free xattr block
        let xattr_block = inode.inode.xattr_block();
        if xattr_block != 0 {
            self.dealloc_block(inode, xattr_block)?;
            inode.inode.set_xattr_block(0);
        }
        // Deallocate the inode
        self.dealloc_inode(inode)?;
        // Invalidate inode cache entry
        self.inode_cache.lock().invalidate(inode_id);
        Ok(())
    }

    fn next_inode_generation(&self, inode_id: InodeId) -> Result<u32> {
        let previous = self.read_inode_uncached(inode_id)?.inode.generation();
        let next = previous.wrapping_add(1);
        Ok(if next == 0 { 1 } else { next })
    }

    fn rollback_new_inode(&self, inode_id: InodeId, is_dir: bool) -> Result<()> {
        // The inode-table slot is the authoritative lifetime identity. It may
        // still contain the previous generation, or the newly initialized
        // generation if the write completed before reporting an error.
        let generation = self.read_inode_uncached(inode_id)?.inode.generation();
        let mut inode = Box::new(Inode::default());
        inode.set_generation(generation);
        inode.set_mode(if is_dir {
            InodeMode::DIRECTORY
        } else {
            InodeMode::FILE
        });
        self.dealloc_inode(&mut InodeRef::new(inode_id, inode))
    }

    /// Physically reclaim the inode lifetime represented by `handle`.
    ///
    /// Validation and reclamation share the inode mutation shard.  The inode is
    /// re-read from disk so an unlink-time value snapshot can never discard
    /// blocks or xattrs added by later writeback.
    pub fn reclaim_inode(
        &self,
        handle: InodeReclaimHandle,
    ) -> core::result::Result<(), InodeReclaimError> {
        match self.reclaim_inode_inner(&handle) {
            Ok(()) => Ok(()),
            Err(error) => Err(InodeReclaimError::new(error, handle)),
        }
    }

    fn reclaim_inode_inner(&self, handle: &InodeReclaimHandle) -> Result<()> {
        self.ensure_mutable()?;
        // Delayed VFS eviction runs after the namespace operation released its
        // guard. Re-enter the transactional domain for the complete multi-
        // transaction reclaim so no legacy direct writer can race snapshots.
        let _metadata_guard = self.lock_transactional_metadata_mutation()?;
        let _mutation_guard =
            self.inode_mutation_locks[self.inode_mutation_lock_index(handle.inode_id)].lock();
        self.reclaim_inode_lifetime(handle.inode_id, handle.generation, self.uses_journal())
    }

    /// Reclaim a mount-recovery orphan without manufacturing a VFS lifetime
    /// capability. The generation is read authoritatively before entering the
    /// same validated orchestration used by delayed final close.
    pub(super) fn reclaim_orphan_inode_by_id(&self, inode_id: InodeId) -> Result<()> {
        self.ensure_mutable()?;
        let _mutation_guard =
            self.inode_mutation_locks[self.inode_mutation_lock_index(inode_id)].lock();
        let generation = self.read_inode_uncached(inode_id)?.inode.generation();
        self.reclaim_inode_lifetime(inode_id, generation, true)
    }

    /// Complete a crash-interrupted truncate for an inode that still has names.
    /// Blocks at or beyond ceil(i_size / block_size) are removed in restartable
    /// transactions; the inode itself and its xattrs remain live.
    pub(super) fn recover_linked_orphan_inode_by_id(&self, inode_id: InodeId) -> Result<()> {
        self.ensure_mutable()?;
        let _mutation_guard =
            self.inode_mutation_locks[self.inode_mutation_lock_index(inode_id)].lock();
        if !self.legacy_orphan_contains(inode_id)? {
            return_error!(ErrCode::EINVAL, "Inode {} is not orphaned", inode_id);
        }
        loop {
            let mut inode = self.read_inode_uncached(inode_id)?;
            let sb = self.read_super_block_cached();
            if !self.inode_is_allocated(inode_id)?
                || inode.inode.mode().bits() == 0
                || inode.inode.link_count() == 0
                || !super::orphan::inode_checksum_valid(&sb, &inode)
                || !inode.inode.is_file()
                || !inode.inode.uses_extents()
            {
                return_error!(ErrCode::EIO, "Invalid linked truncate orphan {}", inode_id);
            }
            let keep_blocks = inode.inode.size().div_ceil(BLOCK_SIZE as u64);
            let mut transaction = self.transaction_start(32)?;
            let Some(tail) = self.extent_tail(&transaction, &inode)? else {
                transaction.abort();
                break;
            };
            let extent_end = tail
                .start_pblock
                .checked_add(tail.block_count as PBlockId)
                .ok_or_else(|| format_error!(ErrCode::EIO, "Invalid extent physical range"))?;
            if tail.start_pblock == 0
                || extent_end > sb.block_count()
                || self.journal_owns_block_range(tail.start_pblock, extent_end)
            {
                return_error!(ErrCode::EIO, "Invalid linked orphan extent");
            }
            let group_limit = extent_tail_batch_limit(
                sb.first_data_block() as PBlockId,
                sb.blocks_per_group() as PBlockId,
                tail.start_pblock,
                tail.block_count,
            )
            .ok_or_else(|| format_error!(ErrCode::EIO, "Invalid extent tail"))?;
            let Some(remove_limit) = linked_orphan_tail_remove_limit(
                keep_blocks,
                tail.start_lblock,
                tail.block_count,
                group_limit,
            ) else {
                transaction.abort();
                break;
            };
            let removed = self
                .extent_remove_tail_in_transaction(&mut transaction, &mut inode, remove_limit)?
                .ok_or_else(|| format_error!(ErrCode::EIO, "Extent tail disappeared"))?;
            self.transaction_dealloc_block_range(
                &mut transaction,
                removed.start_pblock,
                removed.block_count,
            )?;
            for metadata in removed.metadata_blocks.iter().copied() {
                self.transaction_dealloc_block_range(&mut transaction, metadata, 1)?;
            }
            let released = removed.block_count as u64 + removed.metadata_blocks.len() as u64;
            inode.inode.set_fs_block_count(
                inode
                    .inode
                    .fs_block_count()
                    .checked_sub(released)
                    .ok_or_else(|| format_error!(ErrCode::EIO, "Invalid inode block count"))?,
            );
            self.transaction_stage_inode_with_csum(&mut transaction, &mut inode)?;
            self.commit_reclaim_transaction(transaction)?;
        }

        let mut inode = self.read_inode_uncached(inode_id)?;
        if inode.inode.link_count() == 0 {
            return_error!(ErrCode::EIO, "Linked truncate orphan lost all links");
        }
        let mut transaction = self.transaction_start(8)?;
        let mut sb = self.transaction_read_super_block(&transaction)?;
        self.transaction_orphan_del(&mut transaction, &inode, &mut sb)?;
        inode.inode.set_next_orphan(0);
        self.transaction_stage_inode_with_csum(&mut transaction, &mut inode)?;
        self.commit_reclaim_transaction(transaction)
    }

    fn reclaim_inode_lifetime(
        &self,
        inode_id: InodeId,
        generation: u32,
        require_orphan_membership: bool,
    ) -> Result<()> {
        self.validate_reclaim_inode(inode_id, generation)?;
        if require_orphan_membership && !self.legacy_orphan_contains(inode_id)? {
            return_error!(ErrCode::EINVAL, "Inode {} is not orphaned", inode_id);
        }
        // Each iteration starts from the checkpointed inode-table entry.  The
        // on-disk extent root is therefore the restart cursor after any crash.
        // Chain membership was fully validated once above. The metadata write
        // barrier keeps the chain stable, avoiding O(extents * orphan_count)
        // repeated walks; final orphan_del performs its own bounded walk.
        loop {
            let mut inode = self.validate_reclaim_inode(inode_id, generation)?;
            if !inode.inode.uses_extents() {
                if inode.inode.fs_block_count() != 0 {
                    return_error!(ErrCode::EIO, "Non-extent orphan owns blocks");
                }
                break;
            }

            let sb = self.read_super_block_cached();
            let blocks_per_group = sb.blocks_per_group() as PBlockId;
            let first_data_block = sb.first_data_block() as PBlockId;
            let mut transaction = self.transaction_start(32)?;
            let mut batch_group = None;
            let mut removals = 0usize;

            // Sequential range allocation can create hundreds of extents in
            // one block group.  Committing each tail entry independently turns
            // unlink/failed-download cleanup into hundreds of synchronous JBD2
            // barriers.  Bitmap/GDT/superblock and the right-most extent leaf
            // are read-your-writes transaction images, so remove all adjacent
            // tail entries from the same allocation group in one commit.
            while removals < 64 {
                // One removal can detach an extent-tree right spine of at most
                // five blocks. In the adversarial layout every detached
                // metadata block belongs to a different allocation group:
                // data bitmap/GDT (2), five metadata bitmap/GDT pairs (10),
                // one surviving parent (1), the shared superblock (1), and
                // the final inode-table image (1). Do not begin another
                // mutation unless all 15 possible new homes still fit. This
                // avoids a repeatable E2BIG orphan-reclaim failure while
                // retaining large batches for the common shared-home case.
                const NEXT_REMOVAL_CREDIT_RESERVE: usize = 15;
                if removals != 0 && transaction.remaining_credits() < NEXT_REMOVAL_CREDIT_RESERVE {
                    break;
                }
                let Some(tail) = self.extent_tail(&transaction, &inode)? else {
                    break;
                };
                let extent_end = tail
                    .start_pblock
                    .checked_add(tail.block_count as PBlockId)
                    .ok_or_else(|| format_error!(ErrCode::EIO, "Invalid extent physical range"))?;
                if tail.start_pblock == 0
                    || extent_end > sb.block_count()
                    || self.journal_owns_block_range(tail.start_pblock, extent_end)
                {
                    return_error!(
                        ErrCode::EIO,
                        "Orphan extent overlaps invalid or journal-owned blocks"
                    );
                }
                let remove_limit = extent_tail_batch_limit(
                    first_data_block,
                    blocks_per_group,
                    tail.start_pblock,
                    tail.block_count,
                )
                .ok_or_else(|| format_error!(ErrCode::EIO, "Invalid extent tail"))?;
                let removed_first = extent_end
                    .checked_sub(remove_limit as PBlockId)
                    .ok_or_else(|| format_error!(ErrCode::EIO, "Invalid extent tail range"))?;
                let group = removed_first
                    .checked_sub(first_data_block)
                    .ok_or_else(|| format_error!(ErrCode::EIO, "Invalid extent block group"))?
                    / blocks_per_group;
                if batch_group.is_some_and(|current| current != group) {
                    break;
                }
                batch_group = Some(group);

                let removed = self
                    .extent_remove_tail_in_transaction(&mut transaction, &mut inode, remove_limit)?
                    .ok_or_else(|| format_error!(ErrCode::EIO, "Extent tail disappeared"))?;
                self.transaction_dealloc_block_range(
                    &mut transaction,
                    removed.start_pblock,
                    removed.block_count,
                )?;
                for metadata in removed.metadata_blocks.iter().copied() {
                    self.transaction_dealloc_block_range(&mut transaction, metadata, 1)?;
                }
                let released = removed.block_count as u64 + removed.metadata_blocks.len() as u64;
                let remaining = inode
                    .inode
                    .fs_block_count()
                    .checked_sub(released)
                    .ok_or_else(|| format_error!(ErrCode::EIO, "Invalid inode block count"))?;
                inode.inode.set_fs_block_count(remaining);
                inode.inode.set_size(core::cmp::min(
                    inode.inode.size(),
                    removed.start_lblock as u64 * BLOCK_SIZE as u64,
                ));
                removals += 1;
            }
            if removals == 0 {
                transaction.abort();
                break;
            }
            self.transaction_stage_inode_with_csum(&mut transaction, &mut inode)?;
            self.commit_reclaim_transaction(transaction)?;
        }

        // External xattrs form their own restartable transaction. Shared
        // blocks update h_refcount; exclusive blocks also release allocation.
        let mut inode = self.validate_reclaim_inode(inode_id, generation)?;
        if inode.inode.xattr_block() != 0 {
            let mut transaction = self.transaction_start(16)?;
            if let Some(block) = self.transaction_release_xattr(&mut transaction, &mut inode)? {
                self.transaction_dealloc_block_range(&mut transaction, block, 1)?;
            }
            self.commit_reclaim_transaction(transaction)?;
        }

        // Only the final transaction makes the orphan undiscoverable. It also
        // frees the inode number and installs a checksum-correct cleared table
        // entry, preserving generation so reuse advances lifetime identity.
        let inode = self.validate_reclaim_inode(inode_id, generation)?;
        if inode.inode.uses_extents() {
            let transaction = self.transaction_start(1)?;
            if self.extent_tail(&transaction, &inode)?.is_some() {
                return_error!(ErrCode::EIO, "Final reclaim with live extent");
            }
            transaction.abort();
        }
        if inode.inode.xattr_block() != 0 || inode.inode.fs_block_count() != 0 {
            return_error!(ErrCode::EIO, "Final reclaim with owned blocks");
        }
        let is_dir = inode.inode.is_dir();
        let mut transaction = self.transaction_start(16)?;
        if require_orphan_membership {
            let mut sb = self.transaction_read_super_block(&transaction)?;
            self.transaction_orphan_del(&mut transaction, &inode, &mut sb)?;
        }
        self.transaction_dealloc_inode(&mut transaction, inode_id, is_dir)?;
        let mut cleared = InodeRef::new(inode_id, Box::default());
        cleared.inode.set_generation(generation);
        self.transaction_stage_inode_with_csum(&mut transaction, &mut cleared)?;
        self.commit_reclaim_transaction(transaction)
    }

    fn validate_reclaim_inode(&self, inode_id: InodeId, generation: u32) -> Result<InodeRef> {
        if !self.inode_is_allocated(inode_id)? {
            return_error!(
                ErrCode::EINVAL,
                "Reclaim references free inode {}",
                inode_id
            );
        }
        let inode = self.read_inode_uncached(inode_id)?;
        let sb = self.read_super_block_cached();
        if inode.inode.mode().bits() == 0
            || inode.inode.link_count() != 0
            || inode.inode.generation() != generation
            || !super::orphan::inode_checksum_valid(&sb, &inode)
        {
            return_error!(
                ErrCode::EINVAL,
                "Invalid or stale orphan inode {}",
                inode_id
            );
        }
        Ok(inode)
    }

    fn commit_reclaim_transaction(
        &self,
        transaction: super::journal_transaction::Transaction<'_>,
    ) -> Result<()> {
        if let Err(error) = transaction.commit(self.block_device.as_ref(), self) {
            self.poison(ErrCode::EIO);
            return Err(error.error);
        }
        Ok(())
    }

    pub(super) fn inode_is_allocated(&self, inode_id: InodeId) -> Result<bool> {
        let _alloc_guard = self.alloc_lock.lock();
        let sb = self.read_super_block_cached();
        if inode_id == 0 || inode_id > sb.inode_count() {
            return_error!(ErrCode::EINVAL, "Invalid inode number {}", inode_id);
        }
        let inodes_per_group = sb.inodes_per_group();
        let bgid = ((inode_id - 1) / inodes_per_group) as BlockGroupId;
        if bgid >= sb.block_group_count() {
            return_error!(ErrCode::EINVAL, "Invalid inode block group {}", bgid);
        }
        let idx_in_bg = (inode_id - 1) % inodes_per_group;
        let bg = self.read_block_group(bgid)?;
        let bitmap_block = self.read_block(bg.desc.inode_bitmap_block())?;
        let inode_count = sb.inode_count_in_group(bgid) as usize;
        if idx_in_bg as usize >= inode_count {
            return_error!(ErrCode::EINVAL, "Invalid inode index {}", idx_in_bg);
        }
        let mut bitmap_data = bitmap_block.data.clone();
        let bitmap = Bitmap::new(&mut *bitmap_data, inode_count);
        Ok(!bitmap.is_bit_clear(idx_in_bg as usize))
    }

    /// Append a data block for an inode, return a pair of (logical block id, physical block id)
    ///
    /// Only data blocks allocated by `inode_append_block` will be counted in `inode.block_count`.
    /// Blocks allocated by calling `alloc_block` directly will not be counted, i.e., blocks
    /// allocated for the inode's extent tree.
    ///
    /// Appending a block does not increase `inode.size`, because `inode.size` records the actual
    /// size of the data content, not the number of blocks allocated for it.
    ///
    /// If the inode is a file, `inode.size` will be increased when writing to end of the file.
    /// If the inode is a directory, `inode.size` will be increased when adding a new entry to the
    /// newly created block.
    pub(super) fn inode_append_block(&self, inode: &mut InodeRef) -> Result<(LBlockId, PBlockId)> {
        // Determine the next logical block from the extent tree.
        // We cannot use fs_block_count() because i_blocks may include tree
        // metadata blocks (added by setattr after the allocation loop).
        let iblock = self.extent_next_data_lblock(inode)?;
        // Check the extent tree to get the physical block id
        let fblock = self.extent_query_or_create(inode, iblock, 1)?;
        let total_blocks = self
            .extent_all_data_blocks(inode)?
            .len()
            .checked_add(self.extent_all_tree_blocks(inode)?.len())
            .ok_or_else(|| format_error!(ErrCode::EFBIG, "Inode blocks overflow"))?;
        inode.inode.set_fs_block_count(total_blocks as u64);
        self.write_inode_with_csum(inode)?;

        Ok((iblock, fblock))
    }

    /// Allocate a new physical block for an inode, return the physical block number
    pub(super) fn alloc_block(&self, inode: &mut InodeRef) -> Result<PBlockId> {
        let allocation = self.alloc_lock.lock();
        let mut sb = self.read_super_block_cached();
        allocation.ensure_unreserved_capacity(sb.free_blocks_count(), 1)?;
        let inodes_per_group = sb.inodes_per_group();
        let preferred_bgid = ((inode.id - 1) / inodes_per_group) as BlockGroupId;
        let bg_count = sb.block_group_count();

        for i in 0..bg_count {
            let bgid = (preferred_bgid + i) % bg_count;
            let blocks_in_group = Self::block_group_block_count(&sb, bgid);
            if blocks_in_group == 0 {
                continue;
            }

            // Load block group descriptor
            let mut bg = self.read_block_group(bgid)?;
            if bg.desc.get_free_blocks_count() == 0 {
                continue;
            }

            // Load block bitmap. Bits are relative to the start of this block group;
            // extent physical block numbers are absolute filesystem block numbers.
            let bitmap_block_id = bg.desc.block_bitmap_block();
            let mut bitmap_block = self.read_block(bitmap_block_id)?;
            self.prepare_stats.record_bitmap_io();
            let old_bitmap_block = bitmap_block.clone();
            let old_bg = BlockGroupRef::new(bg.id, bg.desc);
            let old_sb = sb;
            let bit = {
                let mut bitmap = Bitmap::new(&mut *bitmap_block.data, blocks_in_group);
                match bitmap.find_and_set_first_clear_bit(0, blocks_in_group) {
                    Some(bit) => bit,
                    None => continue,
                }
            };
            let fblock = Self::block_group_first_block(&sb, bgid) + bit as PBlockId;

            // Set block group checksum
            if !bg.desc.update_block_bitmap_csum(
                sb.metadata_checksum_seed(),
                &*bitmap_block.data,
                sb.clusters_per_group() as usize / 8,
            ) {
                return_error!(ErrCode::EIO, "Invalid block bitmap checksum length");
            }
            self.write_block(&bitmap_block)?;
            self.prepare_stats.record_bitmap_io();

            // Update block group counters
            bg.desc
                .set_free_blocks_count(bg.desc.get_free_blocks_count() - 1);
            if let Err(err) = self.write_block_group_with_csum(&mut bg) {
                return match self.restore_block_allocation_state(
                    &old_bitmap_block,
                    &old_bg,
                    &old_sb,
                ) {
                    Ok(()) => Err(err),
                    Err(rollback_err) => Err(rollback_err),
                };
            }

            // Update superblock counters
            sb.set_free_blocks_count(sb.free_blocks_count() - 1);
            if let Err(err) = self.write_super_block(&sb) {
                return match self.restore_block_allocation_state(
                    &old_bitmap_block,
                    &old_bg,
                    &old_sb,
                ) {
                    Ok(()) => Err(err),
                    Err(rollback_err) => Err(rollback_err),
                };
            }

            trace!("Alloc block {} ok", fblock);
            return Ok(fblock);
        }

        return_error!(ErrCode::ENOSPC, "No free blocks in filesystem");
    }

    /// Allocate and initialize a data block before any extent can publish it.
    /// Extent-tree and xattr metadata allocations deliberately use
    /// `alloc_block()` directly because their callers construct metadata
    /// images rather than exposing zero-filled file data.
    pub(super) fn alloc_zeroed_data_block(&self, inode: &mut InodeRef) -> Result<PBlockId> {
        self.alloc_initialized_data_block(inode, Box::new([0; BLOCK_SIZE]))
    }

    pub(super) fn alloc_initialized_data_block(
        &self,
        inode: &mut InodeRef,
        image: Box<[u8; BLOCK_SIZE]>,
    ) -> Result<PBlockId> {
        let pblock = self.alloc_block(inode)?;
        if let Err(init_error) = self.write_block(&Block::new(pblock, image)) {
            if let Err(rollback_error) = self.dealloc_block(inode, pblock) {
                // The allocation bit may still be set and no extent owns the
                // block.  Fail-stop instead of permitting silent leakage or a
                // later stale-data mapping on this mount.
                self.poison(ErrCode::EIO);
                return Err(rollback_error);
            }
            return Err(init_error);
        }
        self.prepare_stats.record_zero_io();
        Ok(pblock)
    }

    /// Deallocate a physical block allocated for an inode
    pub(super) fn dealloc_block(&self, _inode: &mut InodeRef, pblock: PBlockId) -> Result<()> {
        let _alloc_guard = self.alloc_lock.lock();
        let mut sb = self.read_super_block_cached();
        if pblock >= sb.block_count() {
            return_error!(ErrCode::EINVAL, "Invalid block {}", pblock);
        }

        if pblock < sb.first_data_block() as PBlockId {
            return_error!(ErrCode::EINVAL, "Invalid block {}", pblock);
        }
        let bgid = ((pblock - sb.first_data_block() as PBlockId)
            / sb.blocks_per_group() as PBlockId) as BlockGroupId;
        let bit = (pblock - Self::block_group_first_block(&sb, bgid)) as usize;
        let blocks_in_group = Self::block_group_block_count(&sb, bgid);
        if bit >= blocks_in_group {
            return_error!(ErrCode::EINVAL, "Invalid block {}", pblock);
        }

        // Load block group descriptor
        let mut bg = self.read_block_group(bgid)?;

        // Load block bitmap
        let bitmap_block_id = bg.desc.block_bitmap_block();
        let mut bitmap_block = self.read_block(bitmap_block_id)?;
        let old_bitmap_block = bitmap_block.clone();
        let old_bg = BlockGroupRef::new(bg.id, bg.desc);
        let old_sb = sb;
        {
            let mut bitmap = Bitmap::new(&mut *bitmap_block.data, blocks_in_group);
            // Free the block
            if bitmap.is_bit_clear(bit) {
                return_error!(ErrCode::EINVAL, "Block {} is already free", pblock);
            }
            bitmap.clear_bit(bit);
        }
        // Set block group checksum
        if !bg.desc.update_block_bitmap_csum(
            sb.metadata_checksum_seed(),
            &*bitmap_block.data,
            sb.clusters_per_group() as usize / 8,
        ) {
            return_error!(ErrCode::EIO, "Invalid block bitmap checksum length");
        }
        self.write_block(&bitmap_block)?;

        // Update block group counters
        bg.desc
            .set_free_blocks_count(bg.desc.get_free_blocks_count() + 1);
        if let Err(err) = self.write_block_group_with_csum(&mut bg) {
            return match self.restore_block_allocation_state(&old_bitmap_block, &old_bg, &old_sb) {
                Ok(()) => Err(err),
                Err(rollback_err) => Err(rollback_err),
            };
        }

        // Update superblock counters
        sb.set_free_blocks_count(sb.free_blocks_count() + 1);
        if let Err(err) = self.write_super_block(&sb) {
            return match self.restore_block_allocation_state(&old_bitmap_block, &old_bg, &old_sb) {
                Ok(()) => Err(err),
                Err(rollback_err) => Err(rollback_err),
            };
        }

        trace!("Free block {} ok", pblock);
        Ok(())
    }

    /// Allocate a new inode, returning the inode number.
    fn alloc_inode(&self, is_dir: bool) -> Result<InodeId> {
        let _alloc_guard = self.alloc_lock.lock();
        let mut sb = self.read_super_block_cached();
        let bg_count = sb.block_group_count();

        let mut bgid = 0;
        while bgid < bg_count {
            // Load block group descriptor
            let mut bg = self.read_block_group(bgid)?;
            // If there are no free inodes in this block group, try the next one
            if bg.desc.free_inodes_count() == 0 {
                bgid += 1;
                continue;
            }
            // Load inode bitmap
            let bitmap_block_id = bg.desc.inode_bitmap_block();
            let mut bitmap_block = self.read_block(bitmap_block_id)?;
            let old_bitmap_block = bitmap_block.clone();
            let old_bg = BlockGroupRef::new(bg.id, bg.desc);
            let old_sb = sb;
            let inode_count = sb.inode_count_in_group(bgid) as usize;
            // Find a free inode, limiting allocation to real inodes even though
            // the checksum covers the fixed inodes_per_group bitmap length.
            let idx_in_bg = {
                let mut bitmap = Bitmap::new(&mut *bitmap_block.data, inode_count);
                bitmap
                    .find_and_set_first_clear_bit(0, inode_count)
                    .ok_or(format_error!(
                        ErrCode::ENOSPC,
                        "No free inodes in block group {}",
                        bgid
                    ))? as u32
            };
            // Update bitmap in disk
            if !bg.desc.update_inode_bitmap_csum(
                sb.metadata_checksum_seed(),
                &*bitmap_block.data,
                sb.inodes_per_group() as usize / 8,
            ) {
                return_error!(ErrCode::EIO, "Invalid inode bitmap checksum length");
            }
            self.write_block(&bitmap_block)?;

            // Modify block group counters
            bg.desc
                .set_free_inodes_count(bg.desc.free_inodes_count() - 1);
            if is_dir {
                bg.desc.set_used_dirs_count(bg.desc.used_dirs_count() + 1);
            }
            let mut unused = bg.desc.itable_unused();
            let free = inode_count as u32 - unused;
            if idx_in_bg >= free {
                unused = inode_count as u32 - (idx_in_bg + 1);
                bg.desc.set_itable_unused(unused);
            }
            if let Err(error) = self.write_block_group_with_csum(&mut bg) {
                if self
                    .restore_inode_allocation_state(&old_bitmap_block, &old_bg, &old_sb)
                    .is_err()
                {
                    self.poison(ErrCode::EIO);
                }
                return Err(error);
            }

            // Update superblock counters
            sb.set_free_inodes_count(sb.free_inodes_count() - 1);
            if let Err(error) = self.write_super_block(&sb) {
                if self
                    .restore_inode_allocation_state(&old_bitmap_block, &old_bg, &old_sb)
                    .is_err()
                {
                    self.poison(ErrCode::EIO);
                }
                return Err(error);
            }

            // Compute the absolute i-node number
            let inodes_per_group = sb.inodes_per_group();
            let inode_id = bgid * inodes_per_group + (idx_in_bg + 1);
            return Ok(inode_id);
        }
        trace!("no free inode");
        return_error!(ErrCode::ENOSPC, "No free inodes in block group {}", bgid);
    }

    /// Free an inode
    fn dealloc_inode(&self, inode_ref: &mut InodeRef) -> Result<()> {
        let _alloc_guard = self.alloc_lock.lock();
        let mut sb = self.read_super_block_cached();

        // Calc block group id and index in block group
        let inodes_per_group = sb.inodes_per_group();
        let bgid = ((inode_ref.id - 1) / inodes_per_group) as BlockGroupId;
        let idx_in_bg = (inode_ref.id - 1) % inodes_per_group;
        // Load block group descriptor
        let mut bg = self.read_block_group(bgid)?;
        // Load inode bitmap
        let bitmap_block_id = bg.desc.inode_bitmap_block();
        let mut bitmap_block = self.read_block(bitmap_block_id)?;
        let old_bitmap_block = bitmap_block.clone();
        let old_bg = BlockGroupRef::new(bg.id, bg.desc);
        let old_sb = sb;
        let inode_count = sb.inode_count_in_group(bgid) as usize;
        {
            let mut bitmap = Bitmap::new(&mut *bitmap_block.data, inode_count);
            // Free the inode
            if bitmap.is_bit_clear(idx_in_bg as usize) {
                return_error!(
                    ErrCode::EINVAL,
                    "Inode {} is already free in block group {}",
                    inode_ref.id,
                    bgid
                );
            }
            bitmap.clear_bit(idx_in_bg as usize);
        }
        // Update bitmap in disk
        if !bg.desc.update_inode_bitmap_csum(
            sb.metadata_checksum_seed(),
            &*bitmap_block.data,
            sb.inodes_per_group() as usize / 8,
        ) {
            return_error!(ErrCode::EIO, "Invalid inode bitmap checksum length");
        }
        self.write_block(&bitmap_block)?;

        // Update block group counters
        bg.desc
            .set_free_inodes_count(bg.desc.free_inodes_count() + 1);
        if inode_ref.inode.is_dir() {
            bg.desc.set_used_dirs_count(bg.desc.used_dirs_count() - 1);
        }
        bg.desc.set_itable_unused(bg.desc.itable_unused() + 1);
        if let Err(error) = self.write_block_group_with_csum(&mut bg) {
            if self
                .restore_inode_allocation_state(&old_bitmap_block, &old_bg, &old_sb)
                .is_err()
            {
                self.poison(ErrCode::EIO);
            }
            return Err(error);
        }

        // Update superblock counters
        sb.set_free_inodes_count(sb.free_inodes_count() + 1);
        if let Err(error) = self.write_super_block(&sb) {
            if self
                .restore_inode_allocation_state(&old_bitmap_block, &old_bg, &old_sb)
                .is_err()
            {
                self.poison(ErrCode::EIO);
            }
            return Err(error);
        }

        // Clear inode content while preserving the lifetime generation.  The
        // next allocation advances it before publishing the reused inode.
        let generation = inode_ref.inode.generation();
        *inode_ref.inode = Inode::default();
        inode_ref.inode.set_generation(generation);
        self.write_inode_with_csum(inode_ref)?;

        Ok(())
    }
}

#[cfg(test)]
mod allocation_tests {
    use super::{
        extent_tail_batch_limit, linked_orphan_tail_remove_limit, transaction_range_probe_limit,
        AllocationClass, AllocationState, DelallocReservationUse,
        TRANSACTION_RANGE_MAX_GROUP_PROBES,
    };
    use crate::ErrCode;

    #[test]
    fn delayed_reservation_blocks_unreserved_allocation_until_released() {
        let mut state = AllocationState::new().unwrap();
        let mut reservation = state.reserve_delalloc(8, 5, 2).unwrap();

        assert_eq!(reservation.data_blocks(), 5);
        assert_eq!(reservation.metadata_blocks(), 2);
        assert_eq!(
            state.ensure_unreserved_capacity(8, 2).unwrap_err().code(),
            ErrCode::ENOSPC
        );

        state.release_delalloc(&mut reservation).unwrap();
        state.ensure_unreserved_capacity(8, 8).unwrap();
        assert_eq!(
            state.release_delalloc(&mut reservation).unwrap_err().code(),
            ErrCode::EINVAL
        );
    }

    #[test]
    fn delayed_reservation_rejects_oversell_and_cross_filesystem_release() {
        let mut state_a = AllocationState::new().unwrap();
        let mut state_b = AllocationState::new().unwrap();

        assert_eq!(
            state_a.reserve_delalloc(8, 9, 0).unwrap_err().code(),
            ErrCode::ENOSPC
        );
        assert_eq!(
            state_a.reserve_delalloc(8, 0, 0).unwrap_err().code(),
            ErrCode::EINVAL
        );

        let mut reservation_a = state_a.reserve_delalloc(8, 1, 0).unwrap();
        let mut reservation_b = state_b.reserve_delalloc(8, 8, 0).unwrap();
        assert_eq!(
            state_b
                .release_delalloc(&mut reservation_a)
                .unwrap_err()
                .code(),
            ErrCode::EINVAL
        );
        assert_eq!(
            state_b.ensure_unreserved_capacity(8, 1).unwrap_err().code(),
            ErrCode::ENOSPC
        );

        state_a.release_delalloc(&mut reservation_a).unwrap();
        state_b.release_delalloc(&mut reservation_b).unwrap();
    }

    #[test]
    fn delayed_reservation_consumes_data_and_metadata_separately_with_rollback() {
        let mut state = AllocationState::new().unwrap();
        let mut reservation = state.reserve_delalloc(10, 3, 2).unwrap();
        let class = AllocationClass::Delalloc(reservation.id);

        assert_eq!(
            state
                .consume_delalloc(AllocationClass::Unreserved, DelallocReservationUse::Data, 1)
                .unwrap(),
            None
        );

        let data = state
            .consume_delalloc(class, DelallocReservationUse::Data, 2)
            .unwrap()
            .unwrap();
        assert_eq!(
            state.ensure_unreserved_capacity(10, 8).unwrap_err().code(),
            ErrCode::ENOSPC
        );
        state.rollback_delalloc_consumption(data).unwrap();
        assert_eq!(
            state.ensure_unreserved_capacity(10, 6).unwrap_err().code(),
            ErrCode::ENOSPC
        );

        let data = state
            .consume_delalloc(class, DelallocReservationUse::Data, 3)
            .unwrap()
            .unwrap();
        let metadata = state
            .consume_delalloc(class, DelallocReservationUse::Metadata, 2)
            .unwrap()
            .unwrap();
        assert_eq!(
            state
                .consume_delalloc(class, DelallocReservationUse::Metadata, 1)
                .unwrap_err()
                .code(),
            ErrCode::EIO
        );
        state.commit_delalloc_consumption(data).unwrap();
        state.commit_delalloc_consumption(metadata).unwrap();
        state.finish_delalloc_reservation(&mut reservation).unwrap();
        state.ensure_unreserved_capacity(10, 10).unwrap();
    }

    #[test]
    fn delayed_reservation_rejects_finalisation_or_release_while_consumption_is_in_flight() {
        let mut state = AllocationState::new().unwrap();
        let mut reservation = state.reserve_delalloc(8, 3, 0).unwrap();
        let class = AllocationClass::Delalloc(reservation.id);
        let consumption = state
            .consume_delalloc(class, DelallocReservationUse::Data, 3)
            .unwrap()
            .unwrap();

        assert_eq!(
            state.release_delalloc(&mut reservation).unwrap_err().code(),
            ErrCode::EIO
        );
        assert_eq!(
            state
                .finish_delalloc_reservation(&mut reservation)
                .unwrap_err()
                .code(),
            ErrCode::EIO
        );

        state.rollback_delalloc_consumption(consumption).unwrap();
        assert_eq!(
            state.ensure_unreserved_capacity(8, 6).unwrap_err().code(),
            ErrCode::ENOSPC
        );
        state.release_delalloc(&mut reservation).unwrap();
        state.ensure_unreserved_capacity(8, 8).unwrap();
    }

    #[test]
    fn delayed_reservation_batch_release_is_atomic_on_invalid_lease() {
        let mut state = AllocationState::new().unwrap();
        let mut first = state.reserve_delalloc(16, 3, 1).unwrap();
        let mut second = state.reserve_delalloc(16, 2, 1).unwrap();
        let mut stale = state.reserve_delalloc(16, 5, 2).unwrap();
        state.release_delalloc(&mut stale).unwrap();

        // Queue logical order need not equal opaque reservation serial order:
        // canonicalising inside the ledger must make the reverse input safe.
        state
            .release_delalloc_batch(&mut [&mut second, &mut first])
            .unwrap();
        state.ensure_unreserved_capacity(16, 16).unwrap();

        let mut valid = state.reserve_delalloc(16, 3, 1).unwrap();
        assert_eq!(
            state
                .release_delalloc_batch(&mut [&mut valid, &mut stale])
                .unwrap_err()
                .code(),
            ErrCode::EINVAL
        );
        // The failed batch must not release its valid peer.
        assert_eq!(
            state.ensure_unreserved_capacity(16, 13).unwrap_err().code(),
            ErrCode::ENOSPC
        );
        state.release_delalloc(&mut valid).unwrap();
        state.ensure_unreserved_capacity(16, 16).unwrap();
    }

    #[test]
    fn delayed_reservation_batch_release_rejects_inflight_without_releasing_peers() {
        let mut state = AllocationState::new().unwrap();
        let mut first = state.reserve_delalloc(16, 3, 0).unwrap();
        let mut second = state.reserve_delalloc(16, 4, 0).unwrap();
        let consumption = state
            .consume_delalloc(
                AllocationClass::Delalloc(first.id),
                DelallocReservationUse::Data,
                1,
            )
            .unwrap()
            .unwrap();

        assert_eq!(
            state
                .release_delalloc_batch(&mut [&mut first, &mut second])
                .unwrap_err()
                .code(),
            ErrCode::EIO
        );
        // Neither the in-flight reservation nor its valid peer may have been
        // removed by a failed all-or-nothing batch release.
        assert_eq!(
            state.ensure_unreserved_capacity(16, 11).unwrap_err().code(),
            ErrCode::ENOSPC
        );

        state.rollback_delalloc_consumption(consumption).unwrap();
        state
            .release_delalloc_batch(&mut [&mut first, &mut second])
            .unwrap();
        state.ensure_unreserved_capacity(16, 16).unwrap();
    }

    #[test]
    fn delayed_reservation_batch_release_rejects_committed_partial_claim_without_releasing_peers() {
        let mut state = AllocationState::new().unwrap();
        let mut first = state.reserve_delalloc(16, 3, 0).unwrap();
        let mut second = state.reserve_delalloc(16, 4, 0).unwrap();
        let consumption = state
            .consume_delalloc(
                AllocationClass::Delalloc(first.id),
                DelallocReservationUse::Data,
                1,
            )
            .unwrap()
            .unwrap();
        state.commit_delalloc_consumption(consumption).unwrap();

        assert_eq!(
            state
                .release_delalloc_batch(&mut [&mut first, &mut second])
                .unwrap_err()
                .code(),
            ErrCode::EIO
        );
        // A failed whole-entry release must not turn a partially materialised
        // entry into an implicit "release the remainder" operation, nor
        // release an otherwise valid peer.
        assert_eq!(
            state.ensure_unreserved_capacity(16, 11).unwrap_err().code(),
            ErrCode::ENOSPC
        );
        state.release_delalloc(&mut second).unwrap();
        assert_eq!(
            state.ensure_unreserved_capacity(16, 15).unwrap_err().code(),
            ErrCode::ENOSPC
        );
        let remaining = state
            .consume_delalloc(
                AllocationClass::Delalloc(first.id),
                DelallocReservationUse::Data,
                2,
            )
            .unwrap()
            .unwrap();
        state.commit_delalloc_consumption(remaining).unwrap();
        state.finish_delalloc_reservation(&mut first).unwrap();
        state.ensure_unreserved_capacity(16, 16).unwrap();
    }

    #[test]
    #[should_panic(
        expected = "Delayed-allocation consumption was dropped without commit or rollback"
    )]
    fn delayed_reservation_unresolved_consumption_drop_is_fail_stop() {
        let mut state = AllocationState::new().unwrap();
        let reservation = state.reserve_delalloc(3, 3, 0).unwrap();
        let class = AllocationClass::Delalloc(reservation.id);
        let _consumption = state
            .consume_delalloc(class, DelallocReservationUse::Data, 3)
            .unwrap()
            .unwrap();
        // This test intentionally drops the unresolved debit. Keep the lease
        // alive outside that deliberately failing path so its own fail-stop
        // destructor does not mask the expected assertion.
        core::mem::forget(reservation);
    }

    #[test]
    fn transactional_range_probes_are_bounded_and_exact_requests_are_local() {
        assert_eq!(transaction_range_probe_limit(0, false), 0);
        assert_eq!(transaction_range_probe_limit(1, false), 1);
        assert_eq!(
            transaction_range_probe_limit(128, false),
            TRANSACTION_RANGE_MAX_GROUP_PROBES
        );

        assert_eq!(transaction_range_probe_limit(0, true), 0);
        assert_eq!(transaction_range_probe_limit(1, true), 1);
        assert_eq!(transaction_range_probe_limit(128, true), 1);
    }

    #[test]
    fn extent_reclaim_batch_never_crosses_a_block_group() {
        // 1 KiB ext4 starts data at block 1. The tail spans groups 0 and 1;
        // only the two right-most blocks in group 1 may be removed together.
        assert_eq!(extent_tail_batch_limit(1, 8, 7, 4), Some(2));
        assert_eq!(extent_tail_batch_limit(0, 8, 9, 3), Some(3));
    }

    #[test]
    fn extent_reclaim_batch_rejects_invalid_or_overflowing_tails() {
        assert_eq!(extent_tail_batch_limit(1, 8, 0, 1), None);
        assert_eq!(extent_tail_batch_limit(0, 8, u64::MAX, 2), None);
        assert_eq!(extent_tail_batch_limit(0, 0, 1, 1), None);
    }

    #[test]
    fn linked_orphan_trim_preserves_eof_block_and_honors_group_batch() {
        assert_eq!(linked_orphan_tail_remove_limit(5, 3, 6, 6), Some(4));
        assert_eq!(linked_orphan_tail_remove_limit(5, 3, 6, 2), Some(2));
        assert_eq!(linked_orphan_tail_remove_limit(5, 8, 3, 3), Some(3));
        assert_eq!(linked_orphan_tail_remove_limit(12, 8, 3, 3), None);
    }
}
