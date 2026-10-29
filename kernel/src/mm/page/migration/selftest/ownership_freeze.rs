//! MM-141 regression: a legitimate retained Arc writes before ownership freeze.
//! The old transaction copies too early, then successfully consumes that Page
//! after this owner releases it, losing the latest write in the destination.

use super::*;

pub(super) const OFFSET: usize = 173;
pub(super) const VALUE: u8 = 0xe7;

pub(super) fn latest_guarded_write_survives(details: &mut String) -> bool {
    let Ok(fixture) = Fixture::new(false) else {
        let _ = writeln!(details, "ownership_freeze_stage=fixture");
        return false;
    };
    let Some((old_phys, original_flags)) = fixture.mapping() else {
        return false;
    };
    let Some(source) = fixture.page() else {
        return false;
    };
    let old = Arc::downgrade(&source);
    let control = Control::new(Mode::LateOwnerWrite);
    // Move, rather than clone, the only extra owner into the scheduled writer.
    *control.late_owner.lock() = Some(source);
    let result = migrate(&fixture.mm, fixture.address, &control);
    let wrote = control.late_write_observed.load(Ordering::Acquire);
    let new_mapping = fixture.mapping();
    let data_valid = fixture.page().is_some_and(|page| {
        let data = page.read();
        unsafe { data.as_slice() }
            .iter()
            .enumerate()
            .all(|(index, byte)| {
                *byte
                    == if index == OFFSET {
                        VALUE
                    } else {
                        (index.wrapping_mul(37) ^ (index >> 5)) as u8
                    }
            })
    });
    let committed = result.is_ok()
        && control.committed.load(Ordering::Acquire)
        && new_mapping
            .is_some_and(|(physical, flags)| physical != old_phys && flags == original_flags);
    let retired =
        old.upgrade().is_none() && page_manager_lock().get_registered(&old_phys).is_none();
    let accounted = fixture.account.current_pages() == 1
        && fixture.account.snapshot().anon == 1
        && fixture.mm.resident_pages() == 1;
    let unmapped = fixture.unmap();
    let passed = wrote && committed && data_valid && retired && accounted && unmapped;
    if !passed {
        let _ = writeln!(details,
            "ownership_freeze_stage=checks wrote={wrote} committed={committed} latest_data={data_valid} retired={retired} accounted={accounted} unmapped={unmapped} result={result:?}");
    }
    passed
}
