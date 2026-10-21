#![deny(unsafe_op_in_unsafe_fn)]

use crate::hal::{BufferDirection, Dma, Hal, PhysAddr};
use crate::transport::Transport;
use crate::{align_up, nonnull_slice_from_raw_parts, pages, Error, Result, PAGE_SIZE};
#[cfg(feature = "alloc")]
use alloc::boxed::Box;
use bitflags::bitflags;
#[cfg(test)]
use core::cmp::min;
use core::hint::spin_loop;
use core::mem::{size_of, take};
#[cfg(test)]
use core::ptr;
#[cfg(feature = "alloc")]
use core::ptr::addr_of_mut;
use core::ptr::NonNull;
use core::sync::atomic::{fence, AtomicU16, Ordering};
use zerocopy::{AsBytes, FromBytes, FromZeroes};

/// The mechanism for bulk data transport on virtio devices.
///
/// Each device can have zero or more virtqueues.
///
/// * `SIZE`: The size of the queue. This is both the number of descriptors, and the number of slots
///   in the available and used rings.
#[derive(Debug)]
pub struct VirtQueue<H: Hal, const SIZE: usize> {
    /// DMA guard
    layout: VirtQueueLayout<H>,
    /// Descriptor table
    ///
    /// The device may be able to modify this, even though it's not supposed to, so we shouldn't
    /// trust values read back from it. Use `desc_shadow` instead to keep track of what we wrote to
    /// it.
    desc: NonNull<[Descriptor]>,
    /// Available ring
    ///
    /// The device may be able to modify this, even though it's not supposed to, so we shouldn't
    /// trust values read back from it. The only field we need to read currently is `idx`, so we
    /// have `avail_idx` below to use instead.
    avail: NonNull<AvailRing<SIZE>>,
    /// Used ring
    used: NonNull<UsedRing<SIZE>>,

    /// The index of queue
    queue_idx: u16,
    /// The number of descriptors currently in use.
    num_used: u16,
    /// The head desc index of the free list.
    free_head: u16,
    /// Our trusted copy of `desc` that the device can't access.
    desc_shadow: [Descriptor; SIZE],
    /// Our trusted copy of `avail.idx`.
    avail_idx: u16,
    last_used_idx: u16,
    /// Whether the `VIRTIO_F_EVENT_IDX` feature has been negotiated.
    event_idx: bool,
    /// Whether device-to-driver used-buffer notifications are enabled.
    ///
    /// With EVENT_IDX negotiated, consuming a used entry advances
    /// `used_event` only while this is true. Otherwise pop_used() would
    /// accidentally undo NAPI's notification suppression mid-poll.
    dev_notify_enabled: bool,
    #[cfg(feature = "alloc")]
    indirect: bool,
    #[cfg(feature = "alloc")]
    indirect_lists: [Option<NonNull<[Descriptor]>>; SIZE],
}

impl<H: Hal, const SIZE: usize> VirtQueue<H, SIZE> {
    /// Creates a new VirtQueue.
    ///
    /// * `indirect`: Whether to use indirect descriptors. This should be set if the
    ///   `VIRTIO_F_INDIRECT_DESC` feature has been negotiated with the device.
    /// * `event_idx`: Whether to use the `used_event` and `avail_event` fields for notification
    ///   suppression. This should be set if the `VIRTIO_F_EVENT_IDX` feature has been negotiated
    ///   with the device.
    pub fn new<T: Transport>(
        transport: &mut T,
        idx: u16,
        indirect: bool,
        event_idx: bool,
    ) -> Result<Self> {
        #[cfg(not(feature = "alloc"))]
        let _ = indirect;
        if transport.queue_used(idx) {
            return Err(Error::AlreadyUsed);
        }
        if !SIZE.is_power_of_two()
            || SIZE > u16::MAX.into()
            || transport.max_queue_size(idx) < SIZE as u32
        {
            return Err(Error::InvalidParam);
        }
        let size = SIZE as u16;

        let layout = if transport.requires_legacy_layout() {
            VirtQueueLayout::allocate_legacy(size)?
        } else {
            VirtQueueLayout::allocate_flexible(size)?
        };

        transport.queue_set(
            idx,
            size.into(),
            layout.descriptors_paddr(),
            layout.driver_area_paddr(),
            layout.device_area_paddr(),
        );

        let desc =
            nonnull_slice_from_raw_parts(layout.descriptors_vaddr().cast::<Descriptor>(), SIZE);
        let avail = layout.avail_vaddr().cast();
        let used = layout.used_vaddr().cast();

        let mut desc_shadow: [Descriptor; SIZE] = FromZeroes::new_zeroed();
        // Link descriptors together.
        for i in 0..(size - 1) {
            desc_shadow[i as usize].next = i + 1;
            // Safe because `desc` is properly aligned, dereferenceable, initialised, and the device
            // won't access the descriptors for the duration of this unsafe block.
            unsafe {
                (*desc.as_ptr())[i as usize].next = i + 1;
            }
        }

        #[cfg(feature = "alloc")]
        const NONE: Option<NonNull<[Descriptor]>> = None;
        Ok(VirtQueue {
            layout,
            desc,
            avail,
            used,
            queue_idx: idx,
            num_used: 0,
            free_head: 0,
            desc_shadow,
            avail_idx: 0,
            last_used_idx: 0,
            event_idx,
            dev_notify_enabled: true,
            #[cfg(feature = "alloc")]
            indirect,
            #[cfg(feature = "alloc")]
            indirect_lists: [NONE; SIZE],
        })
    }

    /// Creates a new VirtQueue directly on the heap.
    ///
    /// This avoids placing the full `VirtQueue` value on the caller's stack, which matters for
    /// large queue sizes because the queue contains descriptor shadow arrays sized by `SIZE`.
    #[cfg(feature = "alloc")]
    pub fn new_boxed<T: Transport>(
        transport: &mut T,
        idx: u16,
        indirect: bool,
        event_idx: bool,
    ) -> Result<Box<Self>> {
        if transport.queue_used(idx) {
            return Err(Error::AlreadyUsed);
        }
        if !SIZE.is_power_of_two()
            || SIZE > u16::MAX.into()
            || transport.max_queue_size(idx) < SIZE as u32
        {
            return Err(Error::InvalidParam);
        }
        let size = SIZE as u16;

        let layout = if transport.requires_legacy_layout() {
            VirtQueueLayout::allocate_legacy(size)?
        } else {
            VirtQueueLayout::allocate_flexible(size)?
        };

        transport.queue_set(
            idx,
            size.into(),
            layout.descriptors_paddr(),
            layout.driver_area_paddr(),
            layout.device_area_paddr(),
        );

        let desc =
            nonnull_slice_from_raw_parts(layout.descriptors_vaddr().cast::<Descriptor>(), SIZE);
        let avail = layout.avail_vaddr().cast();
        let used = layout.used_vaddr().cast();

        let mut boxed = Box::<Self>::new_uninit();
        let queue = boxed.as_mut_ptr();

        // SAFETY: `queue` points to a heap allocation for `Self`. Each field is initialized
        // exactly once below before `assume_init`, and no initialized field is read before then.
        unsafe {
            addr_of_mut!((*queue).layout).write(layout);
            addr_of_mut!((*queue).desc).write(desc);
            addr_of_mut!((*queue).avail).write(avail);
            addr_of_mut!((*queue).used).write(used);
            addr_of_mut!((*queue).queue_idx).write(idx);
            addr_of_mut!((*queue).num_used).write(0);
            addr_of_mut!((*queue).free_head).write(0);

            let desc_shadow = addr_of_mut!((*queue).desc_shadow) as *mut Descriptor;
            for i in 0..SIZE {
                let mut shadow: Descriptor = FromZeroes::new_zeroed();
                if i + 1 < SIZE {
                    shadow.next = (i + 1) as u16;
                    (*desc.as_ptr())[i].next = (i + 1) as u16;
                }
                desc_shadow.add(i).write(shadow);
            }

            addr_of_mut!((*queue).avail_idx).write(0);
            addr_of_mut!((*queue).last_used_idx).write(0);
            addr_of_mut!((*queue).event_idx).write(event_idx);
            addr_of_mut!((*queue).indirect).write(indirect);

            let indirect_lists =
                addr_of_mut!((*queue).indirect_lists) as *mut Option<NonNull<[Descriptor]>>;
            for i in 0..SIZE {
                indirect_lists.add(i).write(None);
            }

            Ok(boxed.assume_init())
        }
    }

    /// Add buffers to the virtqueue, return a token.
    ///
    /// The buffers must not be empty.
    ///
    /// Ref: linux virtio_ring.c virtqueue_add
    ///
    /// # Safety
    ///
    /// The input and output buffers must remain valid and not be accessed until a call to
    /// `pop_used` with the returned token succeeds.
    pub unsafe fn add<'a, 'b>(
        &mut self,
        inputs: &'a [&'b [u8]],
        outputs: &'a mut [&'b mut [u8]],
    ) -> Result<u16> {
        if inputs.is_empty() && outputs.is_empty() {
            return Err(Error::InvalidParam);
        }
        let descriptors_needed = inputs
            .len()
            .checked_add(outputs.len())
            .ok_or(Error::InvalidParam)?;
        // Only consider indirect descriptors if the alloc feature is enabled, as they require
        // allocation.
        #[cfg(feature = "alloc")]
        if self.num_used as usize + 1 > SIZE
            || descriptors_needed > SIZE
            || (!self.indirect && self.num_used as usize + descriptors_needed > SIZE)
        {
            return Err(Error::QueueFull);
        }
        #[cfg(not(feature = "alloc"))]
        if self.num_used as usize + descriptors_needed > SIZE {
            return Err(Error::QueueFull);
        }

        if InputOutputIter::new(inputs, outputs)
            .any(|(buffer, _)| buffer.len() == 0 || buffer.len() > u32::MAX as usize)
        {
            return Err(Error::InvalidParam);
        }
        #[cfg(feature = "alloc")]
        let head = if self.indirect && descriptors_needed > 1 {
            self.add_indirect(inputs, outputs)?
        } else {
            self.add_direct(inputs, outputs)?
        };
        #[cfg(not(feature = "alloc"))]
        let head = self.add_direct(inputs, outputs)?;

        let avail_slot = self.avail_idx & (SIZE as u16 - 1);
        // Safe because self.avail is properly aligned, dereferenceable and initialised.
        unsafe {
            (*self.avail.as_ptr()).ring[avail_slot as usize] = head;
        }

        // Write barrier so that device sees changes to descriptor table and available ring before
        // change to available index.
        fence(Ordering::SeqCst);

        // increase head of avail ring
        self.avail_idx = self.avail_idx.wrapping_add(1);
        // Safe because self.avail is properly aligned, dereferenceable and initialised.
        unsafe {
            (*self.avail.as_ptr())
                .idx
                .store(self.avail_idx, Ordering::Release);
        }

        Ok(head)
    }

    fn add_direct<'a, 'b>(
        &mut self,
        inputs: &'a [&'b [u8]],
        outputs: &'a mut [&'b mut [u8]],
    ) -> Result<u16> {
        let head = self.free_head;
        let mut next = head;
        let mut mapped = 0;
        let mut last = head;
        for (buffer, direction) in InputOutputIter::new(inputs, outputs) {
            let desc = &mut self.desc_shadow[usize::from(next)];
            // The caller retains every buffer until pop_used; no descriptor is visible yet.
            if let Err(error) = unsafe { desc.try_set_buf::<H>(buffer, direction, DescFlags::NEXT) }
            {
                let mut rollback = head;
                for (buffer, direction) in InputOutputIter::new(inputs, outputs).take(mapped) {
                    let desc = &mut self.desc_shadow[usize::from(rollback)];
                    unsafe { H::cancel_share(desc.addr as usize, buffer, direction) };
                    desc.unset_buf();
                    desc.flags = DescFlags::empty();
                    rollback = desc.next;
                }
                return Err(error);
            }
            mapped += 1;
            last = next;
            next = desc.next;
        }
        self.desc_shadow[usize::from(last)]
            .flags
            .remove(DescFlags::NEXT);
        let mut index = head;
        for _ in 0..mapped {
            self.write_desc(index);
            index = self.desc_shadow[usize::from(index)].next;
        }
        self.free_head = next;
        self.num_used += mapped as u16;
        Ok(head)
    }

    #[cfg(feature = "alloc")]
    fn add_indirect<'a, 'b>(
        &mut self,
        inputs: &'a [&'b [u8]],
        outputs: &'a mut [&'b mut [u8]],
    ) -> Result<u16> {
        let head = self.free_head;
        let count = inputs.len() + outputs.len();
        let mut indirect_list = Descriptor::try_new_zeroed_list(count)?;
        let mut mapped = 0;
        let mut failure = None;
        for (i, (buffer, direction)) in InputOutputIter::new(inputs, outputs).enumerate() {
            let desc = &mut indirect_list[i];
            if let Err(error) = unsafe { desc.try_set_buf::<H>(buffer, direction, DescFlags::NEXT) }
            {
                failure = Some(error);
                break;
            }
            desc.next = (i + 1) as u16;
            mapped += 1;
        }
        indirect_list
            .last_mut()
            .unwrap()
            .flags
            .remove(DescFlags::NEXT);
        if failure.is_none() {
            // Keep the Box owned until the final mapping also succeeds.
            let desc = &mut self.desc_shadow[usize::from(head)];
            if let Err(error) = unsafe {
                desc.try_set_buf::<H>(
                    indirect_list.as_bytes().into(),
                    BufferDirection::DriverToDevice,
                    DescFlags::INDIRECT,
                )
            } {
                failure = Some(error);
            }
        }
        if let Some(error) = failure {
            for (i, (buffer, direction)) in InputOutputIter::new(inputs, outputs)
                .take(mapped)
                .enumerate()
            {
                unsafe { H::cancel_share(indirect_list[i].addr as usize, buffer, direction) };
            }
            return Err(error);
        }
        assert!(self.indirect_lists[usize::from(head)].is_none());
        self.indirect_lists[usize::from(head)] = Some(Box::leak(indirect_list).into());
        self.free_head = self.desc_shadow[usize::from(head)].next;
        self.write_desc(head);
        self.num_used += 1;
        Ok(head)
    }

    /// Add the given buffers to the virtqueue, notifies the device, blocks until the device uses
    /// them, then pops them.
    ///
    /// This assumes that the device isn't processing any other buffers at the same time.
    ///
    /// The buffers must not be empty.
    pub fn add_notify_wait_pop<'a>(
        &mut self,
        inputs: &'a [&'a [u8]],
        outputs: &'a mut [&'a mut [u8]],
        transport: &mut impl Transport,
    ) -> Result<u32> {
        // Safe because we don't return until the same token has been popped, so the buffers remain
        // valid and are not otherwise accessed until then.
        let token = unsafe { self.add(inputs, outputs) }?;

        // Notify the queue.
        if self.should_notify() {
            transport.notify(self.queue_idx);
        }

        // Wait until there is at least one element in the used ring.
        while !self.can_pop() {
            spin_loop();
        }

        // Safe because these are the same buffers as we passed to `add` above and they are still
        // valid.
        unsafe { self.pop_used(token, inputs, outputs) }
    }

    /// Advise the device whether used buffer notifications are needed.
    ///
    /// See Virtio v1.1 2.6.7 Used Buffer Notification Suppression
    pub fn set_dev_notify(&mut self, enable: bool) {
        if enable {
            let _ = self.enable_dev_notify_prepare();
        } else if self.event_idx {
            self.dev_notify_enabled = false;
            // Ask the device not to interrupt for the next used entry. This is
            // the EVENT_IDX equivalent of setting VRING_AVAIL_F_NO_INTERRUPT.
            unsafe {
                (*self.avail.as_ptr())
                    .used_event
                    .store(self.last_used_idx.wrapping_sub(1), Ordering::Release)
            }
        } else {
            self.dev_notify_enabled = false;
            // Safe because self.avail points to a valid, aligned, initialised, dereferenceable, readable
            // instance of AvailRing.
            unsafe {
                (*self.avail.as_ptr())
                    .flags
                    .store(0x0001, Ordering::Release)
            }
        }
    }

    /// Enable used-buffer notifications and return the current driver-owned
    /// used index for a subsequent race-closing check.
    ///
    /// The caller must pass the returned value to [`Self::dev_notify_pending`]
    /// before trusting that no work arrived during notification enablement.
    pub fn enable_dev_notify_prepare(&mut self) -> u16 {
        self.dev_notify_enabled = true;
        if self.event_idx {
            unsafe {
                (*self.avail.as_ptr())
                    .used_event
                    .store(self.last_used_idx, Ordering::Release)
            }
        } else {
            unsafe {
                (*self.avail.as_ptr())
                    .flags
                    .store(0x0000, Ordering::Release)
            }
        }
        self.last_used_idx
    }

    /// Check whether a used buffer arrived after notifications were enabled.
    ///
    /// The full barrier pairs the driver notification write with the following
    /// device-owned used-index read, matching the virtqueue enable/poll protocol.
    pub fn dev_notify_pending(&self, prepared_used_idx: u16) -> bool {
        fence(Ordering::SeqCst);
        prepared_used_idx != unsafe { (*self.used.as_ptr()).idx.load(Ordering::Acquire) }
    }

    /// Returns whether the driver should notify the device after adding a new buffer to the
    /// virtqueue.
    ///
    /// This will be false if the device has supressed notifications.
    pub fn should_notify(&self) -> bool {
        if self.event_idx {
            // Safe because self.used points to a valid, aligned, initialised, dereferenceable, readable
            // instance of UsedRing.
            let avail_event = unsafe { (*self.used.as_ptr()).avail_event.load(Ordering::Acquire) };
            self.avail_idx >= avail_event.wrapping_add(1)
        } else {
            // Safe because self.used points to a valid, aligned, initialised, dereferenceable, readable
            // instance of UsedRing.
            unsafe { (*self.used.as_ptr()).flags.load(Ordering::Acquire) & 0x0001 == 0 }
        }
    }

    /// Copies the descriptor at the given index from `desc_shadow` to `desc`, so it can be seen by
    /// the device.
    fn write_desc(&mut self, index: u16) {
        let index = usize::from(index);
        // Safe because self.desc is properly aligned, dereferenceable and initialised, and nothing
        // else reads or writes the descriptor during this block.
        unsafe {
            (*self.desc.as_ptr())[index] = self.desc_shadow[index].clone();
        }
    }

    /// Returns whether there is a used element that can be popped.
    pub fn can_pop(&self) -> bool {
        // Safe because self.used points to a valid, aligned, initialised, dereferenceable, readable
        // instance of UsedRing.
        self.last_used_idx != unsafe { (*self.used.as_ptr()).idx.load(Ordering::Acquire) }
    }

    /// Returns the descriptor index (a.k.a. token) of the next used element without popping it, or
    /// `None` if the used ring is empty.
    pub fn peek_used(&self) -> Option<u16> {
        self.peek_used_id()
            .and_then(|id| (id <= u16::MAX.into()).then_some(id as u16))
    }

    /// Returns the untrusted descriptor ID of the next used element without
    /// narrowing the device-provided `u32` value.
    ///
    /// Device drivers which form memory-safety decisions from the token should
    /// validate this value against their queue size before converting it.
    pub fn peek_used_id(&self) -> Option<u32> {
        if self.can_pop() {
            let last_used_slot = self.last_used_idx & (SIZE as u16 - 1);
            // Safe because self.used points to a valid, aligned, initialised, dereferenceable,
            // readable instance of UsedRing.
            Some(unsafe { (*self.used.as_ptr()).ring[last_used_slot as usize].id })
        } else {
            None
        }
    }

    /// Returns the number of free descriptors.
    pub fn available_desc(&self) -> usize {
        #[cfg(feature = "alloc")]
        if self.indirect {
            return if usize::from(self.num_used) == SIZE {
                0
            } else {
                SIZE
            };
        }

        SIZE - usize::from(self.num_used)
    }

    /// Unshares buffers in the list starting at descriptor index `head` and adds them to the free
    /// list. Unsharing may involve copying data back to the original buffers, so they must be
    /// passed in too.
    ///
    /// This will push all linked descriptors at the front of the free list.
    ///
    /// # Safety
    ///
    /// The buffers in `inputs` and `outputs` must match the set of buffers originally added to the
    /// queue by `add`.
    unsafe fn recycle_descriptors<'a>(
        &mut self,
        head: u16,
        inputs: &'a [&'a [u8]],
        outputs: &'a mut [&'a mut [u8]],
    ) {
        let original_free_head = self.free_head;
        self.free_head = head;

        let head_desc = &mut self.desc_shadow[usize::from(head)];
        if head_desc.flags.contains(DescFlags::INDIRECT) {
            #[cfg(feature = "alloc")]
            {
                // Find the indirect descriptor list, unshare it and move its descriptor to the free
                // list.
                let indirect_list = self.indirect_lists[usize::from(head)].take().unwrap();
                // SAFETY: We allocated the indirect list in `add_indirect`, and the device has
                // finished accessing it by this point.
                let mut indirect_list = unsafe { Box::from_raw(indirect_list.as_ptr()) };
                let paddr = head_desc.addr;
                head_desc.unset_buf();
                self.num_used -= 1;
                head_desc.next = original_free_head;

                unsafe {
                    H::unshare(
                        paddr as usize,
                        indirect_list.as_bytes_mut().into(),
                        BufferDirection::DriverToDevice,
                    );
                }

                // Unshare the buffers in the indirect descriptor list, and free it.
                assert_eq!(indirect_list.len(), inputs.len() + outputs.len());
                for (i, (buffer, direction)) in InputOutputIter::new(inputs, outputs).enumerate() {
                    assert_ne!(buffer.len(), 0);

                    // SAFETY: The caller ensures that the buffer is valid and matches the
                    // descriptor from which we got `paddr`.
                    unsafe {
                        // Unshare the buffer (and perhaps copy its contents back to the original
                        // buffer).
                        H::unshare(indirect_list[i].addr as usize, buffer, direction);
                    }
                }
                drop(indirect_list);
            }
        } else {
            let mut next = Some(head);

            for (buffer, direction) in InputOutputIter::new(inputs, outputs) {
                assert_ne!(buffer.len(), 0);

                let desc_index = next.expect("Descriptor chain was shorter than expected.");
                let desc = &mut self.desc_shadow[usize::from(desc_index)];

                let paddr = desc.addr;
                desc.unset_buf();
                self.num_used -= 1;
                next = desc.next();
                if next.is_none() {
                    desc.next = original_free_head;
                }

                self.write_desc(desc_index);

                // SAFETY: The caller ensures that the buffer is valid and matches the descriptor
                // from which we got `paddr`.
                unsafe {
                    // Unshare the buffer (and perhaps copy its contents back to the original buffer).
                    H::unshare(paddr as usize, buffer, direction);
                }
            }

            if next.is_some() {
                panic!("Descriptor chain was longer than expected.");
            }
        }
    }

    /// Recycles a descriptor chain that will never appear in the used ring.
    ///
    /// This is intended for driver teardown after the caller has reset the device and confirmed
    /// that it can no longer access the queue. It is not a replacement for `pop_used` on the normal
    /// completion path.
    ///
    /// # Safety
    ///
    /// The device must have stopped accessing the queue, and the buffers in `inputs` and `outputs`
    /// must match the set of buffers originally added to the queue by `add` when it returned
    /// `token`.
    pub unsafe fn detach_unused<'a>(
        &mut self,
        token: u16,
        inputs: &'a [&'a [u8]],
        outputs: &'a mut [&'a mut [u8]],
    ) -> Result<()> {
        if usize::from(token) >= SIZE {
            return Err(Error::InvalidParam);
        }

        // SAFETY: The caller guarantees the device is no longer accessing the queue and the
        // buffers match the descriptor chain identified by `token`.
        unsafe {
            self.recycle_descriptors(token, inputs, outputs);
        }
        Ok(())
    }

    /// If the given token is next on the device used queue, pops it and returns the total buffer
    /// length which was used (written) by the device.
    ///
    /// Ref: linux virtio_ring.c virtqueue_get_buf_ctx
    ///
    /// # Safety
    ///
    /// The buffers in `inputs` and `outputs` must match the set of buffers originally added to the
    /// queue by `add` when it returned the token being passed in here.
    pub unsafe fn pop_used<'a>(
        &mut self,
        token: u16,
        inputs: &'a [&'a [u8]],
        outputs: &'a mut [&'a mut [u8]],
    ) -> Result<u32> {
        if !self.can_pop() {
            return Err(Error::NotReady);
        }

        // Get the index of the start of the descriptor chain for the next element in the used ring.
        let last_used_slot = self.last_used_idx & (SIZE as u16 - 1);
        let device_index;
        let len;
        // Safe because self.used points to a valid, aligned, initialised, dereferenceable, readable
        // instance of UsedRing.
        unsafe {
            device_index = (*self.used.as_ptr()).ring[last_used_slot as usize].id;
            len = (*self.used.as_ptr()).ring[last_used_slot as usize].len;
        }

        if device_index >= SIZE as u32 {
            return Err(Error::IoError);
        }
        let index = device_index as u16;
        if index != token {
            // The device used a different descriptor chain to the one we were expecting.
            return Err(Error::WrongToken);
        }

        // Safe because the caller ensures the buffers are valid and match the descriptor.
        unsafe {
            self.recycle_descriptors(index, inputs, outputs);
        }
        self.last_used_idx = self.last_used_idx.wrapping_add(1);

        if self.event_idx && self.dev_notify_enabled {
            unsafe {
                (*self.avail.as_ptr())
                    .used_event
                    .store(self.last_used_idx, Ordering::Release);
            }
        }

        Ok(len)
    }
}

/// The inner layout of a VirtQueue.
///
/// Ref: 2.6 Split Virtqueues
#[derive(Debug)]
enum VirtQueueLayout<H: Hal> {
    Legacy {
        dma: Dma<H>,
        avail_offset: usize,
        used_offset: usize,
    },
    Modern {
        /// Descriptors, with the available ring packed behind them only when
        /// doing so does not increase the physical allocation's order.
        driver_to_device_dma: Dma<H>,
        /// Separate available ring for page-sized or larger descriptor tables.
        /// Small queues retain their original two-region memory footprint.
        avail_dma: Option<Dma<H>>,
        /// The region used for the device area.
        device_to_driver_dma: Dma<H>,
        /// The offset from the start of the `driver_to_device_dma` region to the driver area
        /// (available ring).
        avail_offset: usize,
    },
}

impl<H: Hal> VirtQueueLayout<H> {
    /// Allocates a single DMA region containing all parts of the virtqueue, following the layout
    /// required by legacy interfaces.
    ///
    /// Ref: 2.6.2 Legacy Interfaces: A Note on Virtqueue Layout
    fn allocate_legacy(queue_size: u16) -> Result<Self> {
        let (desc, avail, used) = queue_part_sizes(queue_size);
        let size = align_up(desc + avail) + align_up(used);
        // Allocate contiguous pages.
        let dma = Dma::new(size / PAGE_SIZE, BufferDirection::Both)?;
        Ok(Self::Legacy {
            dma,
            avail_offset: desc,
            used_offset: align_up(desc + avail),
        })
    }

    /// Allocates separate DMA regions for the the different parts of the virtqueue, as supported by
    /// non-legacy interfaces.
    ///
    /// This is preferred over `allocate_legacy` where possible as it reduces memory fragmentation
    /// and allows the HAL to know which DMA regions are used in which direction.
    fn allocate_flexible(queue_size: u16) -> Result<Self> {
        let (desc, avail, used) = queue_part_sizes(queue_size);
        // Modern transports supply three independent base addresses. Do not
        // manufacture a higher-order requirement by packing an available ring
        // after a descriptor table which already fills complete pages.
        let separate_avail = desc >= PAGE_SIZE && pages(desc + avail) > pages(desc);
        let driver_to_device_dma = Dma::new(
            pages(if separate_avail { desc } else { desc + avail }),
            BufferDirection::DriverToDevice,
        )?;
        let avail_dma = if separate_avail {
            Some(Dma::new(pages(avail), BufferDirection::DriverToDevice)?)
        } else {
            None
        };
        let device_to_driver_dma = Dma::new(pages(used), BufferDirection::DeviceToDriver)?;
        Ok(Self::Modern {
            driver_to_device_dma,
            avail_dma,
            device_to_driver_dma,
            avail_offset: if separate_avail { 0 } else { desc },
        })
    }

    /// Returns the physical address of the descriptor area.
    fn descriptors_paddr(&self) -> PhysAddr {
        match self {
            Self::Legacy { dma, .. } => dma.paddr(),
            Self::Modern {
                driver_to_device_dma,
                ..
            } => driver_to_device_dma.paddr(),
        }
    }

    /// Returns a pointer to the descriptor table (in the descriptor area).
    fn descriptors_vaddr(&self) -> NonNull<u8> {
        match self {
            Self::Legacy { dma, .. } => dma.vaddr(0),
            Self::Modern {
                driver_to_device_dma,
                ..
            } => driver_to_device_dma.vaddr(0),
        }
    }

    /// Returns the physical address of the driver area.
    fn driver_area_paddr(&self) -> PhysAddr {
        match self {
            Self::Legacy {
                dma, avail_offset, ..
            } => dma.paddr() + avail_offset,
            Self::Modern {
                driver_to_device_dma,
                avail_dma,
                avail_offset,
                ..
            } => avail_dma.as_ref().unwrap_or(driver_to_device_dma).paddr() + avail_offset,
        }
    }

    /// Returns a pointer to the available ring (in the driver area).
    fn avail_vaddr(&self) -> NonNull<u8> {
        match self {
            Self::Legacy {
                dma, avail_offset, ..
            } => dma.vaddr(*avail_offset),
            Self::Modern {
                driver_to_device_dma,
                avail_dma,
                avail_offset,
                ..
            } => avail_dma
                .as_ref()
                .unwrap_or(driver_to_device_dma)
                .vaddr(*avail_offset),
        }
    }

    /// Returns the physical address of the device area.
    fn device_area_paddr(&self) -> PhysAddr {
        match self {
            Self::Legacy {
                used_offset, dma, ..
            } => dma.paddr() + used_offset,
            Self::Modern {
                device_to_driver_dma,
                ..
            } => device_to_driver_dma.paddr(),
        }
    }

    /// Returns a pointer to the used ring (in the driver area).
    fn used_vaddr(&self) -> NonNull<u8> {
        match self {
            Self::Legacy {
                dma, used_offset, ..
            } => dma.vaddr(*used_offset),
            Self::Modern {
                device_to_driver_dma,
                ..
            } => device_to_driver_dma.vaddr(0),
        }
    }
}

/// Returns the size in bytes of the descriptor table, available ring and used ring for a given
/// queue size.
///
/// Ref: 2.6 Split Virtqueues
fn queue_part_sizes(queue_size: u16) -> (usize, usize, usize) {
    assert!(
        queue_size.is_power_of_two(),
        "queue size should be a power of 2"
    );
    let queue_size = queue_size as usize;
    let desc = size_of::<Descriptor>() * queue_size;
    let avail = size_of::<u16>() * (3 + queue_size);
    let used = size_of::<u16>() * 3 + size_of::<UsedElem>() * queue_size;
    (desc, avail, used)
}

#[repr(C, align(16))]
#[derive(AsBytes, Clone, Debug, FromBytes, FromZeroes)]
pub(crate) struct Descriptor {
    addr: u64,
    len: u32,
    flags: DescFlags,
    next: u16,
}

impl Descriptor {
    /// Sets the buffer address, length and flags, and shares it with the device.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the buffer lives at least as long as the descriptor is active.
    unsafe fn try_set_buf<H: Hal>(
        &mut self,
        buf: NonNull<[u8]>,
        direction: BufferDirection,
        extra_flags: DescFlags,
    ) -> Result {
        if buf.len() == 0 || buf.len() > u32::MAX as usize {
            return Err(Error::InvalidParam);
        }
        // Safe because our caller promises that the buffer is valid.
        unsafe {
            self.addr = H::try_share(buf, direction)? as u64;
        }
        self.len = buf.len() as u32;
        self.flags = extra_flags
            | match direction {
                BufferDirection::DeviceToDriver => DescFlags::WRITE,
                BufferDirection::DriverToDevice => DescFlags::empty(),
                BufferDirection::Both => {
                    panic!("Buffer passed to device should never use BufferDirection::Both.")
                }
            };
        Ok(())
    }

    #[cfg(feature = "alloc")]
    fn try_new_zeroed_list(count: usize) -> Result<Box<[Self]>> {
        let layout = core::alloc::Layout::array::<Self>(count).map_err(|_| Error::InvalidParam)?;
        // Descriptor is FromZeroes; count is nonzero and its layout is checked above.
        let raw = unsafe { alloc::alloc::alloc_zeroed(layout) } as *mut Self;
        let raw = NonNull::new(raw).ok_or(Error::DmaError)?;
        Ok(unsafe { Box::from_raw(core::ptr::slice_from_raw_parts_mut(raw.as_ptr(), count)) })
    }

    /// Sets the buffer address and length to 0.
    ///
    /// This must only be called once the device has finished using the descriptor.
    fn unset_buf(&mut self) {
        self.addr = 0;
        self.len = 0;
    }

    /// Returns the index of the next descriptor in the chain if the `NEXT` flag is set, or `None`
    /// if it is not (and thus this descriptor is the end of the chain).
    fn next(&self) -> Option<u16> {
        if self.flags.contains(DescFlags::NEXT) {
            Some(self.next)
        } else {
            None
        }
    }
}

/// Descriptor flags
#[derive(AsBytes, Copy, Clone, Debug, Default, Eq, FromBytes, FromZeroes, PartialEq)]
#[repr(transparent)]
struct DescFlags(u16);

bitflags! {
    impl DescFlags: u16 {
        const NEXT = 1;
        const WRITE = 2;
        const INDIRECT = 4;
    }
}

/// The driver uses the available ring to offer buffers to the device:
/// each ring entry refers to the head of a descriptor chain.
/// It is only written by the driver and read by the device.
#[repr(C)]
#[derive(Debug)]
struct AvailRing<const SIZE: usize> {
    flags: AtomicU16,
    /// A driver MUST NOT decrement the idx.
    idx: AtomicU16,
    ring: [u16; SIZE],
    /// Only used if `VIRTIO_F_EVENT_IDX` is negotiated.
    used_event: AtomicU16,
}

/// The used ring is where the device returns buffers once it is done with them:
/// it is only written to by the device, and read by the driver.
#[repr(C)]
#[derive(Debug)]
struct UsedRing<const SIZE: usize> {
    flags: AtomicU16,
    idx: AtomicU16,
    ring: [UsedElem; SIZE],
    /// Only used if `VIRTIO_F_EVENT_IDX` is negotiated.
    avail_event: AtomicU16,
}

#[repr(C)]
#[derive(Debug)]
struct UsedElem {
    id: u32,
    len: u32,
}

struct InputOutputIter<'a, 'b> {
    inputs: &'a [&'b [u8]],
    outputs: &'a mut [&'b mut [u8]],
}

impl<'a, 'b> InputOutputIter<'a, 'b> {
    fn new(inputs: &'a [&'b [u8]], outputs: &'a mut [&'b mut [u8]]) -> Self {
        Self { inputs, outputs }
    }
}

impl<'a, 'b> Iterator for InputOutputIter<'a, 'b> {
    type Item = (NonNull<[u8]>, BufferDirection);

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(input) = take_first(&mut self.inputs) {
            Some(((*input).into(), BufferDirection::DriverToDevice))
        } else {
            let output = take_first_mut(&mut self.outputs)?;
            Some(((*output).into(), BufferDirection::DeviceToDriver))
        }
    }
}

// TODO: Use `slice::take_first` once it is stable
// (https://github.com/rust-lang/rust/issues/62280).
fn take_first<'a, T>(slice: &mut &'a [T]) -> Option<&'a T> {
    let (first, rem) = slice.split_first()?;
    *slice = rem;
    Some(first)
}

// TODO: Use `slice::take_first_mut` once it is stable
// (https://github.com/rust-lang/rust/issues/62280).
fn take_first_mut<'a, T>(slice: &mut &'a mut [T]) -> Option<&'a mut T> {
    let (first, rem) = take(slice).split_first_mut()?;
    *slice = rem;
    Some(first)
}

/// Simulates the device reading from a VirtIO queue and writing a response back, for use in tests.
///
/// The fake device always uses descriptors in order.
#[cfg(test)]
pub(crate) fn fake_read_write_queue<const QUEUE_SIZE: usize>(
    descriptors: *const [Descriptor; QUEUE_SIZE],
    queue_driver_area: *const u8,
    queue_device_area: *mut u8,
    handler: impl FnOnce(Vec<u8>) -> Vec<u8>,
) {
    use core::{ops::Deref, slice};

    let available_ring = queue_driver_area as *const AvailRing<QUEUE_SIZE>;
    let used_ring = queue_device_area as *mut UsedRing<QUEUE_SIZE>;

    // Safe because the various pointers are properly aligned, dereferenceable, initialised, and
    // nothing else accesses them during this block.
    unsafe {
        // Make sure there is actually at least one descriptor available to read from.
        assert_ne!(
            (*available_ring).idx.load(Ordering::Acquire),
            (*used_ring).idx.load(Ordering::Acquire)
        );
        // The fake device always uses descriptors in order, like VIRTIO_F_IN_ORDER, so
        // `used_ring.idx` marks the next descriptor we should take from the available ring.
        let next_slot = (*used_ring).idx.load(Ordering::Acquire) & (QUEUE_SIZE as u16 - 1);
        let head_descriptor_index = (*available_ring).ring[next_slot as usize];
        let mut descriptor = &(*descriptors)[head_descriptor_index as usize];

        let input_length;
        let output;
        if descriptor.flags.contains(DescFlags::INDIRECT) {
            // The descriptor shouldn't have any other flags if it is indirect.
            assert_eq!(descriptor.flags, DescFlags::INDIRECT);

            // Loop through all input descriptors in the indirect descriptor list, reading data from
            // them.
            let indirect_descriptor_list: &[Descriptor] = zerocopy::Ref::new_slice(
                slice::from_raw_parts(descriptor.addr as *const u8, descriptor.len as usize),
            )
            .unwrap()
            .into_slice();
            let mut input = Vec::new();
            let mut indirect_descriptor_index = 0;
            while indirect_descriptor_index < indirect_descriptor_list.len() {
                let indirect_descriptor = &indirect_descriptor_list[indirect_descriptor_index];
                if indirect_descriptor.flags.contains(DescFlags::WRITE) {
                    break;
                }

                input.extend_from_slice(slice::from_raw_parts(
                    indirect_descriptor.addr as *const u8,
                    indirect_descriptor.len as usize,
                ));

                indirect_descriptor_index += 1;
            }
            input_length = input.len();

            // Let the test handle the request.
            output = handler(input);

            // Write the response to the remaining descriptors.
            let mut remaining_output = output.deref();
            while indirect_descriptor_index < indirect_descriptor_list.len() {
                let indirect_descriptor = &indirect_descriptor_list[indirect_descriptor_index];
                assert!(indirect_descriptor.flags.contains(DescFlags::WRITE));

                let length_to_write = min(remaining_output.len(), indirect_descriptor.len as usize);
                ptr::copy(
                    remaining_output.as_ptr(),
                    indirect_descriptor.addr as *mut u8,
                    length_to_write,
                );
                remaining_output = &remaining_output[length_to_write..];

                indirect_descriptor_index += 1;
            }
            assert_eq!(remaining_output.len(), 0);
        } else {
            // Loop through all input descriptors in the chain, reading data from them.
            let mut input = Vec::new();
            while !descriptor.flags.contains(DescFlags::WRITE) {
                input.extend_from_slice(slice::from_raw_parts(
                    descriptor.addr as *const u8,
                    descriptor.len as usize,
                ));

                if let Some(next) = descriptor.next() {
                    descriptor = &(*descriptors)[next as usize];
                } else {
                    break;
                }
            }
            input_length = input.len();

            // Let the test handle the request.
            output = handler(input);

            // Write the response to the remaining descriptors.
            let mut remaining_output = output.deref();
            if descriptor.flags.contains(DescFlags::WRITE) {
                loop {
                    assert!(descriptor.flags.contains(DescFlags::WRITE));

                    let length_to_write = min(remaining_output.len(), descriptor.len as usize);
                    ptr::copy(
                        remaining_output.as_ptr(),
                        descriptor.addr as *mut u8,
                        length_to_write,
                    );
                    remaining_output = &remaining_output[length_to_write..];

                    if let Some(next) = descriptor.next() {
                        descriptor = &(*descriptors)[next as usize];
                    } else {
                        break;
                    }
                }
            }
            assert_eq!(remaining_output.len(), 0);
        }

        // Mark the buffer as used.
        (*used_ring).ring[next_slot as usize].id = head_descriptor_index as u32;
        (*used_ring).ring[next_slot as usize].len = (input_length + output.len()) as u32;
        (*used_ring).idx.fetch_add(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        device::common::Feature,
        hal::fake::FakeHal,
        transport::{
            fake::{FakeTransport, QueueStatus, State},
            mmio::{MmioTransport, VirtIOHeader, MODERN_VERSION},
            DeviceType,
        },
    };
    use core::ptr::NonNull;
    use std::sync::{Arc, Mutex};

    // Thread-local injection keeps parallel tests independent while exercising the real add path.
    std::thread_local! {
        static SHARE_STATE: core::cell::Cell<(usize, usize, usize)> = const { core::cell::Cell::new((0, 0, 0)) };
    }

    #[derive(Debug)]
    struct FailingShareHal;

    unsafe impl Hal for FailingShareHal {
        fn dma_alloc(pages: usize, direction: BufferDirection) -> (usize, NonNull<u8>) {
            FakeHal::dma_alloc(pages, direction)
        }
        unsafe fn dma_dealloc(paddr: usize, vaddr: NonNull<u8>, pages: usize) -> i32 {
            unsafe { FakeHal::dma_dealloc(paddr, vaddr, pages) }
        }
        unsafe fn mmio_phys_to_virt(paddr: usize, size: usize) -> NonNull<u8> {
            unsafe { FakeHal::mmio_phys_to_virt(paddr, size) }
        }
        unsafe fn share(buffer: NonNull<[u8]>, direction: BufferDirection) -> usize {
            unsafe { FakeHal::share(buffer, direction) }
        }
        unsafe fn try_share(buffer: NonNull<[u8]>, direction: BufferDirection) -> Result<usize> {
            SHARE_STATE.with(|state| {
                let (attempts, fail_at, live) = state.get();
                state.set((attempts + 1, fail_at, live));
                if attempts + 1 == fail_at {
                    return Err(Error::DmaError);
                }
                let paddr = unsafe { FakeHal::share(buffer, direction) };
                state.set((attempts + 1, fail_at, live + 1));
                Ok(paddr)
            })
        }
        unsafe fn unshare(paddr: usize, buffer: NonNull<[u8]>, direction: BufferDirection) {
            SHARE_STATE.with(|state| {
                let (attempts, fail_at, live) = state.get();
                state.set((attempts, fail_at, live.checked_sub(1).unwrap()));
            });
            unsafe { FakeHal::unshare(paddr, buffer, direction) }
        }
        unsafe fn cancel_share(paddr: usize, buffer: NonNull<[u8]>, _direction: BufferDirection) {
            // No device has written a cancelled output; preserve the original contents.
            unsafe { Self::unshare(paddr, buffer, BufferDirection::DriverToDevice) }
        }
    }

    #[test]
    fn mapping_failure_rolls_back_direct_and_indirect() {
        for indirect in [false, true] {
            if indirect && !cfg!(feature = "alloc") {
                continue;
            }
            // Two inputs and two outputs exercise failure after an output was already mapped.
            for fail_at in 1..=if indirect { 5 } else { 4 } {
                SHARE_STATE.with(|state| state.set((0, fail_at, 0)));
                let mut header = VirtIOHeader::make_fake_header(MODERN_VERSION, 1, 0, 0, 8);
                let mut transport =
                    unsafe { MmioTransport::new(NonNull::from(&mut header)) }.unwrap();
                let mut queue =
                    VirtQueue::<FailingShareHal, 8>::new(&mut transport, 0, indirect, false)
                        .unwrap();
                let input1 = [1, 2];
                let input2 = [3, 4];
                let inputs = [&input1[..], &input2[..]];
                let mut output1 = [0xa5; 3];
                let mut output2 = [0x5a; 2];
                let original_free_head = queue.free_head;
                assert_eq!(
                    unsafe { queue.add(&inputs, &mut [&mut output1, &mut output2]) },
                    Err(Error::DmaError)
                );
                assert_eq!(SHARE_STATE.with(|state| state.get().2), 0);
                assert_eq!(output1, [0xa5; 3]);
                assert_eq!(output2, [0x5a; 2]);
                assert_eq!(queue.free_head, original_free_head);
                assert_eq!(queue.num_used, 0);
                assert_eq!(queue.avail_idx, 0);
                assert_eq!(
                    unsafe { (*queue.avail.as_ptr()).idx.load(Ordering::Acquire) },
                    0
                );
                #[cfg(feature = "alloc")]
                assert!(queue.indirect_lists.iter().all(Option::is_none));

                SHARE_STATE.with(|state| state.set((0, 0, 0)));
                let token =
                    unsafe { queue.add(&inputs, &mut [&mut output1, &mut output2]) }.unwrap();
                unsafe {
                    (*queue.used.as_ptr()).ring[0].id = token as u32;
                    (*queue.used.as_ptr()).ring[0].len = 0;
                    (*queue.used.as_ptr()).idx.store(1, Ordering::Release);
                    queue
                        .pop_used(token, &inputs, &mut [&mut output1, &mut output2])
                        .unwrap();
                }
                assert_eq!(SHARE_STATE.with(|state| state.get().2), 0);
                assert_eq!(queue.num_used, 0);
            }
        }
    }

    #[test]
    fn invalid_queue_size() {
        let mut header = VirtIOHeader::make_fake_header(MODERN_VERSION, 1, 0, 0, 4);
        let mut transport = unsafe { MmioTransport::new(NonNull::from(&mut header)) }.unwrap();
        // Size not a power of 2.
        assert_eq!(
            VirtQueue::<FakeHal, 3>::new(&mut transport, 0, false, false).unwrap_err(),
            Error::InvalidParam
        );
    }

    #[test]
    fn queue_too_big() {
        let mut header = VirtIOHeader::make_fake_header(MODERN_VERSION, 1, 0, 0, 4);
        let mut transport = unsafe { MmioTransport::new(NonNull::from(&mut header)) }.unwrap();
        assert_eq!(
            VirtQueue::<FakeHal, 8>::new(&mut transport, 0, false, false).unwrap_err(),
            Error::InvalidParam
        );
    }

    #[test]
    fn queue_already_used() {
        let mut header = VirtIOHeader::make_fake_header(MODERN_VERSION, 1, 0, 0, 4);
        let mut transport = unsafe { MmioTransport::new(NonNull::from(&mut header)) }.unwrap();
        VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, false).unwrap();
        assert_eq!(
            VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, false).unwrap_err(),
            Error::AlreadyUsed
        );
    }

    #[test]
    fn add_empty() {
        let mut header = VirtIOHeader::make_fake_header(MODERN_VERSION, 1, 0, 0, 4);
        let mut transport = unsafe { MmioTransport::new(NonNull::from(&mut header)) }.unwrap();
        let mut queue = VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, false).unwrap();
        assert_eq!(
            unsafe { queue.add(&[], &mut []) }.unwrap_err(),
            Error::InvalidParam
        );
    }

    #[test]
    fn add_too_many() {
        let mut header = VirtIOHeader::make_fake_header(MODERN_VERSION, 1, 0, 0, 4);
        let mut transport = unsafe { MmioTransport::new(NonNull::from(&mut header)) }.unwrap();
        let mut queue = VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, false).unwrap();
        assert_eq!(queue.available_desc(), 4);
        assert_eq!(
            unsafe { queue.add(&[&[], &[], &[]], &mut [&mut [], &mut []]) }.unwrap_err(),
            Error::QueueFull
        );
    }

    #[test]
    fn add_buffers() {
        let mut header = VirtIOHeader::make_fake_header(MODERN_VERSION, 1, 0, 0, 4);
        let mut transport = unsafe { MmioTransport::new(NonNull::from(&mut header)) }.unwrap();
        let mut queue = VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, false).unwrap();
        assert_eq!(queue.available_desc(), 4);

        // Add a buffer chain consisting of two device-readable parts followed by two
        // device-writable parts.
        let token = unsafe { queue.add(&[&[1, 2], &[3]], &mut [&mut [0, 0], &mut [0]]) }.unwrap();

        assert_eq!(queue.available_desc(), 0);
        assert!(!queue.can_pop());

        // Safe because the various parts of the queue are properly aligned, dereferenceable and
        // initialised, and nothing else is accessing them at the same time.
        unsafe {
            let first_descriptor_index = (*queue.avail.as_ptr()).ring[0];
            assert_eq!(first_descriptor_index, token);
            assert_eq!(
                (*queue.desc.as_ptr())[first_descriptor_index as usize].len,
                2
            );
            assert_eq!(
                (*queue.desc.as_ptr())[first_descriptor_index as usize].flags,
                DescFlags::NEXT
            );
            let second_descriptor_index =
                (*queue.desc.as_ptr())[first_descriptor_index as usize].next;
            assert_eq!(
                (*queue.desc.as_ptr())[second_descriptor_index as usize].len,
                1
            );
            assert_eq!(
                (*queue.desc.as_ptr())[second_descriptor_index as usize].flags,
                DescFlags::NEXT
            );
            let third_descriptor_index =
                (*queue.desc.as_ptr())[second_descriptor_index as usize].next;
            assert_eq!(
                (*queue.desc.as_ptr())[third_descriptor_index as usize].len,
                2
            );
            assert_eq!(
                (*queue.desc.as_ptr())[third_descriptor_index as usize].flags,
                DescFlags::NEXT | DescFlags::WRITE
            );
            let fourth_descriptor_index =
                (*queue.desc.as_ptr())[third_descriptor_index as usize].next;
            assert_eq!(
                (*queue.desc.as_ptr())[fourth_descriptor_index as usize].len,
                1
            );
            assert_eq!(
                (*queue.desc.as_ptr())[fourth_descriptor_index as usize].flags,
                DescFlags::WRITE
            );
        }
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn add_buffers_indirect() {
        use core::ptr::slice_from_raw_parts;

        let mut header = VirtIOHeader::make_fake_header(MODERN_VERSION, 1, 0, 0, 4);
        let mut transport = unsafe { MmioTransport::new(NonNull::from(&mut header)) }.unwrap();
        let mut queue = VirtQueue::<FakeHal, 4>::new(&mut transport, 0, true, false).unwrap();
        assert_eq!(queue.available_desc(), 4);

        // Add a buffer chain consisting of two device-readable parts followed by two
        // device-writable parts.
        let token = unsafe { queue.add(&[&[1, 2], &[3]], &mut [&mut [0, 0], &mut [0]]) }.unwrap();

        assert_eq!(queue.available_desc(), 4);
        assert!(!queue.can_pop());

        // Safe because the various parts of the queue are properly aligned, dereferenceable and
        // initialised, and nothing else is accessing them at the same time.
        unsafe {
            let indirect_descriptor_index = (*queue.avail.as_ptr()).ring[0];
            assert_eq!(indirect_descriptor_index, token);
            assert_eq!(
                (*queue.desc.as_ptr())[indirect_descriptor_index as usize].len as usize,
                4 * size_of::<Descriptor>()
            );
            assert_eq!(
                (*queue.desc.as_ptr())[indirect_descriptor_index as usize].flags,
                DescFlags::INDIRECT
            );

            let indirect_descriptors = slice_from_raw_parts(
                (*queue.desc.as_ptr())[indirect_descriptor_index as usize].addr
                    as *const Descriptor,
                4,
            );
            assert_eq!((*indirect_descriptors)[0].len, 2);
            assert_eq!((*indirect_descriptors)[0].flags, DescFlags::NEXT);
            assert_eq!((*indirect_descriptors)[0].next, 1);
            assert_eq!((*indirect_descriptors)[1].len, 1);
            assert_eq!((*indirect_descriptors)[1].flags, DescFlags::NEXT);
            assert_eq!((*indirect_descriptors)[1].next, 2);
            assert_eq!((*indirect_descriptors)[2].len, 2);
            assert_eq!(
                (*indirect_descriptors)[2].flags,
                DescFlags::NEXT | DescFlags::WRITE
            );
            assert_eq!((*indirect_descriptors)[2].next, 3);
            assert_eq!((*indirect_descriptors)[3].len, 1);
            assert_eq!((*indirect_descriptors)[3].flags, DescFlags::WRITE);
        }
    }

    /// Tests that the queue advises the device that notifications are needed.
    #[test]
    fn set_dev_notify() {
        let mut config_space = ();
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default()],
            ..Default::default()
        }));
        let mut transport = FakeTransport {
            device_type: DeviceType::Block,
            max_queue_size: 4,
            device_features: 0,
            config_space: NonNull::from(&mut config_space),
            state: state.clone(),
        };
        let mut queue = VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, false).unwrap();

        // Check that the avail ring's flag is zero by default.
        assert_eq!(
            unsafe { (*queue.avail.as_ptr()).flags.load(Ordering::Acquire) },
            0x0
        );

        queue.set_dev_notify(false);

        // Check that the avail ring's flag is 1 after `disable_dev_notify`.
        assert_eq!(
            unsafe { (*queue.avail.as_ptr()).flags.load(Ordering::Acquire) },
            0x1
        );

        queue.set_dev_notify(true);

        // Check that the avail ring's flag is 0 after `enable_dev_notify`.
        assert_eq!(
            unsafe { (*queue.avail.as_ptr()).flags.load(Ordering::Acquire) },
            0x0
        );

        let prepared = queue.enable_dev_notify_prepare();
        assert_eq!(prepared, 0);
        assert!(!queue.dev_notify_pending(prepared));

        // Model one device completion between callback enablement and the
        // driver's race-closing check.
        unsafe {
            (*queue.used.as_ptr()).ring[0].id = u16::MAX as u32 + 1;
            (*queue.used.as_ptr()).idx.store(1, Ordering::Release);
        }
        assert!(queue.dev_notify_pending(prepared));
        assert_eq!(queue.peek_used_id(), Some(u16::MAX as u32 + 1));
        assert_eq!(queue.peek_used(), None);
    }

    /// Tests notification disable/enable semantics with EVENT_IDX negotiated.
    #[test]
    fn set_dev_notify_event_idx() {
        let mut config_space = ();
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default()],
            ..Default::default()
        }));
        let mut transport = FakeTransport {
            device_type: DeviceType::Block,
            max_queue_size: 4,
            device_features: 0,
            config_space: NonNull::from(&mut config_space),
            state,
        };
        let mut queue = VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, true).unwrap();

        queue.set_dev_notify(false);
        assert_eq!(
            unsafe { (*queue.avail.as_ptr()).used_event.load(Ordering::Acquire) },
            u16::MAX
        );

        let prepared = queue.enable_dev_notify_prepare();
        assert_eq!(prepared, 0);
        assert_eq!(
            unsafe { (*queue.avail.as_ptr()).used_event.load(Ordering::Acquire) },
            0
        );
        assert!(!queue.dev_notify_pending(prepared));

        unsafe {
            (*queue.used.as_ptr()).idx.store(1, Ordering::Release);
        }
        assert!(queue.dev_notify_pending(prepared));
    }

    /// Consuming completions while NAPI-style notification suppression is
    /// active must not advance used_event and reopen the interrupt stream.
    #[test]
    fn pop_used_preserves_event_idx_suppression() {
        let mut config_space = ();
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default()],
            ..Default::default()
        }));
        let mut transport = FakeTransport {
            device_type: DeviceType::Block,
            max_queue_size: 4,
            device_features: 0,
            config_space: NonNull::from(&mut config_space),
            state,
        };
        let mut queue = VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, true).unwrap();
        let mut output = [0u8; 4];
        let token = unsafe { queue.add(&[], &mut [&mut output]) }.unwrap();
        unsafe {
            (*queue.used.as_ptr()).ring[0].id = token as u32;
            (*queue.used.as_ptr()).ring[0].len = output.len() as u32;
            (*queue.used.as_ptr()).idx.store(1, Ordering::Release);
        }

        queue.set_dev_notify(false);
        assert_eq!(
            unsafe { queue.pop_used(token, &[], &mut [&mut output]) }.unwrap(),
            output.len() as u32
        );
        assert_eq!(
            unsafe { (*queue.avail.as_ptr()).used_event.load(Ordering::Acquire) },
            u16::MAX
        );
    }

    /// The full device-provided u32 ID consumed by pop_used must be validated
    /// before it is narrowed to the driver's u16 descriptor token.
    #[test]
    fn pop_used_rejects_out_of_range_device_id() {
        let mut config_space = ();
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default()],
            ..Default::default()
        }));
        let mut transport = FakeTransport {
            device_type: DeviceType::Block,
            max_queue_size: 4,
            device_features: 0,
            config_space: NonNull::from(&mut config_space),
            state,
        };
        let mut queue = VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, true).unwrap();
        let mut output = [0u8; 4];
        let token = unsafe { queue.add(&[], &mut [&mut output]) }.unwrap();
        unsafe {
            (*queue.used.as_ptr()).ring[0].id = u16::MAX as u32 + 1;
            (*queue.used.as_ptr()).ring[0].len = output.len() as u32;
            (*queue.used.as_ptr()).idx.store(1, Ordering::Release);
        }

        assert_eq!(
            unsafe { queue.pop_used(token, &[], &mut [&mut output]) }.unwrap_err(),
            Error::IoError
        );
        assert_eq!(queue.last_used_idx, 0);
        assert_eq!(queue.available_desc(), 3);
    }

    /// Tests that the queue notifies the device about added buffers, if it hasn't suppressed
    /// notifications.
    #[test]
    fn add_notify() {
        let mut config_space = ();
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default()],
            ..Default::default()
        }));
        let mut transport = FakeTransport {
            device_type: DeviceType::Block,
            max_queue_size: 4,
            device_features: 0,
            config_space: NonNull::from(&mut config_space),
            state: state.clone(),
        };
        let mut queue = VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, false).unwrap();

        // Add a buffer chain with a single device-readable part.
        unsafe { queue.add(&[&[42]], &mut []) }.unwrap();

        // Check that the transport would be notified.
        assert_eq!(queue.should_notify(), true);

        // SAFETY: the various parts of the queue are properly aligned, dereferenceable and
        // initialised, and nothing else is accessing them at the same time.
        unsafe {
            // Suppress notifications.
            (*queue.used.as_ptr()).flags.store(0x01, Ordering::Release);
        }

        // Check that the transport would not be notified.
        assert_eq!(queue.should_notify(), false);
    }

    /// Tests that the queue notifies the device about added buffers, if it hasn't suppressed
    /// notifications with the `avail_event` index.
    #[test]
    fn add_notify_event_idx() {
        let mut config_space = ();
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default()],
            ..Default::default()
        }));
        let mut transport = FakeTransport {
            device_type: DeviceType::Block,
            max_queue_size: 4,
            device_features: Feature::RING_EVENT_IDX.bits(),
            config_space: NonNull::from(&mut config_space),
            state: state.clone(),
        };
        let mut queue = VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, true).unwrap();

        // Add a buffer chain with a single device-readable part.
        assert_eq!(unsafe { queue.add(&[&[42]], &mut []) }.unwrap(), 0);

        // Check that the transport would be notified.
        assert_eq!(queue.should_notify(), true);

        // SAFETY: the various parts of the queue are properly aligned, dereferenceable and
        // initialised, and nothing else is accessing them at the same time.
        unsafe {
            // Suppress notifications.
            (*queue.used.as_ptr())
                .avail_event
                .store(1, Ordering::Release);
        }

        // Check that the transport would not be notified.
        assert_eq!(queue.should_notify(), false);

        // Add another buffer chain.
        assert_eq!(unsafe { queue.add(&[&[42]], &mut []) }.unwrap(), 1);

        // Check that the transport should be notified again now.
        assert_eq!(queue.should_notify(), true);
    }
}
