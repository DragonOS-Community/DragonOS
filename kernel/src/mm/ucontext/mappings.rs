use super::*;

/// User space mapping information
#[derive(Clone, Copy, Debug)]
pub(super) struct MmapReservation {
    id: MmapReservationId,
    region: VirtRegion,
}

#[derive(Debug)]
pub struct UserMappings {
    /// Virtual Memory Areas (VMAs) in the current user space
    pub(super) vmas: HashSet<Arc<LockedVMA>>,
    /// VMAs indexed by start address, used for address lookup, range scanning, and deletion.
    pub(super) vmas_by_start: BTreeMap<VirtAddr, Arc<LockedVMA>>,
    /// VMA holes in the current user space
    pub(super) vm_holes: BTreeMap<VirtAddr, usize>,
    /// mmap address reservations that are being set up but not yet published as VMAs.
    pub(super) reservations: BTreeMap<VirtAddr, MmapReservation>,
    /// Owning address space, used to back-fill the reverse reference during VMA lifecycle changes
    owner: Weak<AddressSpace>,
}

impl UserMappings {
    pub fn new() -> Self {
        return Self {
            vmas: HashSet::new(),
            vmas_by_start: BTreeMap::new(),
            vm_holes: core::iter::once((VirtAddr::new(0), MMArch::USER_END_VADDR.data()))
                .collect::<BTreeMap<_, _>>(),
            reservations: BTreeMap::new(),
            owner: Weak::new(),
        };
    }

    pub fn set_owner(&mut self, owner: Weak<AddressSpace>) {
        self.owner = owner;
        for vma in self.vmas.iter() {
            self.attach_vma(vma);
        }
    }

    fn attach_vma(&self, vma: &Arc<LockedVMA>) {
        let vm_file = {
            let mut guard = vma.lock();
            if guard.user_address_space.is_none() {
                guard.user_address_space = Some(self.owner.clone());
            }
            guard.vm_file.clone()
        };
        if let Some(file) = vm_file {
            if let Some(page_cache) = file.inode().page_cache() {
                page_cache.register_file_vma(vma);
            }
        }
    }

    fn detach_vma(&self, vma: &Arc<LockedVMA>) {
        let vm_file = { vma.lock().vm_file.clone() };
        if let Some(file) = vm_file {
            if let Some(page_cache) = file.inode().page_cache() {
                page_cache.unregister_file_vma(vma.id());
            }
        }
    }

    /// Check whether any VMA in the current process contains the specified virtual address.
    ///
    /// Returns the Arc pointer of the VMA containing the address if found, otherwise returns None.
    #[allow(dead_code)]
    pub fn contains(&self, vaddr: VirtAddr) -> Option<Arc<LockedVMA>> {
        let (_, vma) = self.vmas_by_start.range(..=vaddr).next_back()?;
        if vma.lock().region.contains(vaddr) {
            Some(vma.clone())
        } else {
            None
        }
    }

    /// Find the VMA nearest to the given virtual address.
    /// ## Parameters
    ///
    /// - `vaddr`: Virtual address
    ///
    /// ## Returns
    /// - Some(Arc<LockedVMA>): The VMA containing the address or the nearest subsequent VMA
    /// - None: No VMA found
    #[allow(dead_code)]
    pub fn find_nearest(&self, vaddr: VirtAddr) -> Option<Arc<LockedVMA>> {
        if let Some(vma) = self.contains(vaddr) {
            return Some(vma);
        }
        self.vmas_by_start
            .range(vaddr..)
            .next()
            .map(|(_, vma)| vma.clone())
    }

    /// Get all VMAs in the current process's address space that overlap with the given virtual address range.
    pub fn conflicts(&self, request: VirtRegion) -> Vec<Arc<LockedVMA>> {
        let mut result = Vec::new();
        if let Some((start, vma)) = self.vmas_by_start.range(..=request.start()).next_back() {
            if *start < request.start() && vma.lock().region.intersect(&request).is_some() {
                result.push(vma.clone());
            }
        }
        for (_start, vma) in self.vmas_by_start.range(request.start()..request.end()) {
            if vma.lock().region.intersect(&request).is_some() {
                result.push(vma.clone());
            }
        }
        result
    }

    pub fn has_conflict(&self, request: VirtRegion) -> bool {
        if let Some((start, vma)) = self.vmas_by_start.range(..=request.start()).next_back() {
            if *start < request.start() && vma.lock().region.intersect(&request).is_some() {
                return true;
            }
        }
        self.vmas_by_start
            .range(request.start()..request.end())
            .any(|(_start, vma)| vma.lock().region.intersect(&request).is_some())
    }

    pub fn conflicts_with_unmapped(&self, request: VirtRegion) -> (Vec<Arc<LockedVMA>>, bool) {
        let conflicts = self.conflicts(request);
        let mut cursor = request.start();
        let mut has_unmapped = false;

        for vma in &conflicts {
            let vma_region = *vma.lock().region();
            if vma_region.start() > cursor {
                has_unmapped = true;
            }
            if vma_region.end() > cursor {
                cursor = cmp::min(vma_region.end(), request.end());
            }
        }
        if cursor < request.end() {
            has_unmapped = true;
        }

        (conflicts, has_unmapped)
    }

    pub fn iter_vmas_starting_at(
        &self,
        start: VirtAddr,
    ) -> impl Iterator<Item = Arc<LockedVMA>> + '_ {
        self.vmas_by_start
            .range(start..)
            .map(|(_start, vma)| vma.clone())
    }

    pub fn first_reservation_conflict(&self, request: VirtRegion) -> Option<MmapReservationId> {
        self.reservations
            .values()
            .find(|reservation| reservation.region.collide(&request))
            .map(|reservation| reservation.id)
    }

    pub fn first_reservation_region(&self) -> Option<VirtRegion> {
        self.reservations
            .values()
            .next()
            .map(|reservation| reservation.region)
    }

    pub(super) fn reservation_usage_bytes(&self) -> usize {
        self.reservations
            .values()
            .map(|reservation| reservation.region.size())
            .sum()
    }

    fn region_available_for_reservation(&self, region: VirtRegion) -> bool {
        !self.has_conflict(region) && self.first_reservation_conflict(region).is_none()
    }

    pub(super) fn reserve_region(
        &mut self,
        region: VirtRegion,
    ) -> Result<MmapReservationId, SystemError> {
        if !self.region_available_for_reservation(region) {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }

        let id = MMAP_RESERVATION_ID_ALLOCATOR.fetch_add(1, Ordering::Relaxed);
        self.reserve_hole(&region);
        self.reservations
            .insert(region.start(), MmapReservation { id, region });
        Ok(id)
    }

    pub(super) fn cancel_reservation(&mut self, id: MmapReservationId) -> Option<VirtRegion> {
        let start = self
            .reservations
            .iter()
            .find_map(|(start, reservation)| (reservation.id == id).then_some(*start))?;
        let reservation = self.reservations.remove(&start)?;
        self.unreserve_hole(&reservation.region);
        Some(reservation.region)
    }

    pub(super) fn remove_reservation_for_commit(
        &mut self,
        id: MmapReservationId,
        region: VirtRegion,
    ) -> Result<(), SystemError> {
        let start = self
            .reservations
            .iter()
            .find_map(|(start, reservation)| (reservation.id == id).then_some(*start))
            .ok_or(SystemError::EFAULT)?;
        let reservation = *self.reservations.get(&start).ok_or(SystemError::EFAULT)?;
        if reservation.region != region {
            return Err(SystemError::EFAULT);
        }
        self.reservations.remove(&start);
        Ok(())
    }

    pub(super) fn commit_reserved_vma(
        &mut self,
        id: MmapReservationId,
        vma: Arc<LockedVMA>,
    ) -> Result<(), SystemError> {
        let region = vma.lock().region;
        self.remove_reservation_for_commit(id, region)?;
        self.insert_vma(vma);
        Ok(())
    }

    /// Find the first free virtual memory region in the current process's address space
    /// that satisfies the given constraints.
    ///
    /// @param min_vaddr Minimum start address
    /// @param size Requested size
    ///
    /// @return The virtual memory region if found, otherwise None
    pub fn find_free(&self, min_vaddr: VirtAddr, req_size: usize) -> Option<VirtRegion> {
        let mut iter = self
            .vm_holes
            .iter()
            .skip_while(|(hole_vaddr, hole_size)| hole_vaddr.add(**hole_size) <= min_vaddr);

        let (hole_vaddr, _hole_size) = iter.find(|(hole_vaddr, hole_size)| {
            // Compute the available size of the current hole
            let available_size: usize =
                if hole_vaddr <= &&min_vaddr && min_vaddr <= hole_vaddr.add(**hole_size) {
                    **hole_size - (min_vaddr - **hole_vaddr)
                } else {
                    **hole_size
                };

            req_size <= available_size
        })?;

        // Return a region exactly equal to the requested size; the start address is the larger of the hole start and the minimum address.
        let region = VirtRegion::new(cmp::max(*hole_vaddr, min_vaddr), req_size);

        return Some(region);
    }

    /// Reserve a region of the specified size in the current process's address space,
    /// removing it from the hole list.
    /// This function modifies the hole information in vm_holes.
    ///
    /// @param region The region to reserve
    ///
    /// Note: before calling this function, you must ensure that there are no VMAs within the region.
    fn reserve_hole(&mut self, region: &VirtRegion) {
        let prev_hole: Option<(&VirtAddr, &mut usize)> =
            self.vm_holes.range_mut(..=region.start()).next_back();

        if let Some((prev_hole_vaddr, prev_hole_size)) = prev_hole {
            let prev_hole_end = prev_hole_vaddr.add(*prev_hole_size);

            if prev_hole_end > region.start() {
                // If the previous hole extends past the start of the current hole, adjust the previous hole's size.
                *prev_hole_size = region.start().data() - prev_hole_vaddr.data();
            }

            if prev_hole_end > region.end() {
                // If the previous hole extends past the end of the current hole, insert a new hole.
                self.vm_holes
                    .insert(region.end(), prev_hole_end - region.end());
            }
        }
    }

    /// Release a region of the specified size in the current process's address space,
    /// making it a hole in the address space.
    /// This function modifies the hole information in vm_holes.
    fn unreserve_hole(&mut self, region: &VirtRegion) {
        // If the hole to be inserted is adjacent to the next hole, merge them.
        let next_hole_size: Option<usize> = self.vm_holes.remove(&region.end());

        if let Some((_prev_hole_vaddr, prev_hole_size)) = self
            .vm_holes
            .range_mut(..region.start())
            .next_back()
            .filter(|(offset, size)| offset.data() + **size == region.start().data())
        {
            *prev_hole_size += region.size() + next_hole_size.unwrap_or(0);
        } else {
            self.vm_holes
                .insert(region.start(), region.size() + next_hole_size.unwrap_or(0));
        }
    }

    /// Insert a new VMA into the current process's mappings.
    pub fn insert_vma(&mut self, vma: Arc<LockedVMA>) {
        let region = vma.lock().region;
        // The address range to be inserted must be free, meaning no overlapping VMA may exist in the current process's address space.
        assert!(!self.has_conflict(region));
        self.reserve_hole(&region);

        self.attach_vma(&vma);
        self.vmas_by_start.insert(region.start(), vma.clone());
        self.vmas.insert(vma);
    }

    /// Remove a VMA from the current mappings and add the corresponding address space to the hole list.
    ///
    /// This does not unmap the addresses corresponding to the VMA, i.e. it does not modify the process page table.
    ///
    /// ### Parameters
    ///  region The address range of the VMA to remove
    ///
    /// ### Returns
    /// - The removed VMA on success, otherwise None.
    /// - If there is no removable VMA matching the region, no deletion is performed and failure is reported.
    ///
    /// ### Side effects
    /// - Modifies the hole information in vm_holes.
    ///
    pub fn remove_vma(&mut self, region: &VirtRegion) -> Option<Arc<LockedVMA>> {
        let vma = self.vmas_by_start.remove(&region.start())?;
        if vma.lock().region != *region {
            self.vmas_by_start.insert(region.start(), vma);
            return None;
        }
        let removed = self.vmas.remove(&vma);
        debug_assert!(removed, "vmas_by_start and vmas diverged for {:?}", region);
        self.unreserve_hole(region);
        self.detach_vma(&vma);

        return Some(vma);
    }

    pub(super) fn take_all_vmas(&mut self) -> Vec<Arc<LockedVMA>> {
        let vmas = self.vmas.drain().collect::<Vec<_>>();
        self.vmas_by_start.clear();
        self.reservations.clear();
        self.vm_holes.clear();
        self.vm_holes
            .insert(VirtAddr::new(0), MMArch::USER_END_VADDR.data());
        for vma in &vmas {
            self.detach_vma(vma);
        }
        vmas
    }

    /// @brief Get the iterator of all VMAs in this process.
    pub fn iter_vmas(&self) -> hashbrown::hash_set::Iter<'_, Arc<LockedVMA>> {
        return self.vmas.iter();
    }
}

impl Default for UserMappings {
    fn default() -> Self {
        return Self::new();
    }
}
