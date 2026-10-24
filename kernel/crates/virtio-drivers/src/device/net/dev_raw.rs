use super::{
    Config, EthernetAddress, Features, HeaderLayout, ReceiveMetadata, ReceiveParser, RxOffloads,
    MIN_GSO_RX_BUFFER_LEN,
};
use super::{
    MIN_BUFFER_LEN, MODERN_NET_HDR_SIZE, QUEUE_RECEIVE, QUEUE_TRANSMIT, SUPPORTED_FEATURES,
};
use crate::hal::Hal;
use crate::queue::VirtQueue;
use crate::transport::Transport;
use crate::volatile::volread;
use crate::{Error, Result};
#[cfg(feature = "alloc")]
use alloc::boxed::Box;
use log::{debug, info, warn};

/// Raw driver for a VirtIO network device.
///
/// This is a raw version of the VirtIONet driver. It provides non-blocking
/// methods for transmitting and receiving raw slices, without the buffer
/// management. For more higher-level fucntions such as receive buffer backing,
/// see [`VirtIONet`].
///
/// [`VirtIONet`]: super::VirtIONet
pub struct VirtIONetRaw<H: Hal, T: Transport, const QUEUE_SIZE: usize> {
    transport: T,
    mac: EthernetAddress,
    header_layout: HeaderLayout,
    guest_csum: bool,
    rx_offloads: RxOffloads,
    stopped: bool,
    #[cfg(feature = "alloc")]
    recv_queue: Box<VirtQueue<H, QUEUE_SIZE>>,
    #[cfg(not(feature = "alloc"))]
    recv_queue: VirtQueue<H, QUEUE_SIZE>,
    #[cfg(feature = "alloc")]
    send_queue: Box<VirtQueue<H, QUEUE_SIZE>>,
    #[cfg(not(feature = "alloc"))]
    send_queue: VirtQueue<H, QUEUE_SIZE>,
}

/// Queue positions captured while enabling device-to-driver notifications.
///
/// Pass this value to [`VirtIONetRaw::interrupt_pending`] immediately after
/// enabling notifications to close the disable/poll/enable race.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InterruptState {
    receive_used_idx: u16,
    transmit_used_idx: u16,
}

impl<H: Hal, T: Transport, const QUEUE_SIZE: usize> VirtIONetRaw<H, T, QUEUE_SIZE> {
    /// Create a new VirtIO-Net driver.
    pub fn new(transport: T) -> Result<Self> {
        Self::new_with_rx_offloads(transport, RxOffloads::empty())
    }

    /// Enable only receive segmentation formats the caller can consume through
    /// ReceiveParser metadata. Ordinary packet APIs continue to reject GSO.
    pub fn new_with_rx_offloads(mut transport: T, offloads: RxOffloads) -> Result<Self> {
        if offloads.contains(RxOffloads::ECN)
            && !offloads.intersects(RxOffloads::TCPV4 | RxOffloads::TCPV6)
        {
            return Err(Error::InvalidParam);
        }
        let negotiated_features =
            transport.try_begin_init(SUPPORTED_FEATURES | offloads.features())?;
        let rx_offloads = RxOffloads::negotiated(negotiated_features);
        if (!rx_offloads.is_empty() && !negotiated_features.contains(Features::GUEST_CSUM))
            || (rx_offloads.contains(RxOffloads::ECN)
                && !rx_offloads.intersects(RxOffloads::TCPV4 | RxOffloads::TCPV6))
        {
            Self::stop_transport(&mut transport);
            return Err(Error::Unsupported);
        }
        let header_layout = if negotiated_features.contains(Features::VERSION_1) {
            HeaderLayout::Modern
        } else {
            HeaderLayout::Legacy
        };
        info!("negotiated_features {:?}", negotiated_features);
        // This driver exposes the device-provided MAC and has no control-queue
        // mechanism to install a locally generated address.
        if !negotiated_features.contains(Features::MAC) {
            Self::stop_transport(&mut transport);
            return Err(Error::Unsupported);
        }
        // read configuration space
        let config = transport.config_space::<Config>()?;
        let mac;
        // Safe because config points to a valid MMIO region for the config space.
        unsafe {
            mac = volread!(config, mac);
            debug!("Got MAC={:02x?}", mac);
            if negotiated_features.contains(Features::STATUS) {
                debug!("Link status={:?}", volread!(config, status));
            }
        }
        #[cfg(feature = "alloc")]
        let send_queue = VirtQueue::new_boxed(
            &mut transport,
            QUEUE_TRANSMIT,
            false,
            negotiated_features.contains(Features::RING_EVENT_IDX),
        )?;
        #[cfg(not(feature = "alloc"))]
        let send_queue = VirtQueue::new(
            &mut transport,
            QUEUE_TRANSMIT,
            false,
            negotiated_features.contains(Features::RING_EVENT_IDX),
        )?;
        #[cfg(feature = "alloc")]
        let recv_queue = VirtQueue::new_boxed(
            &mut transport,
            QUEUE_RECEIVE,
            false,
            negotiated_features.contains(Features::RING_EVENT_IDX),
        );
        #[cfg(not(feature = "alloc"))]
        let recv_queue = VirtQueue::new(
            &mut transport,
            QUEUE_RECEIVE,
            false,
            negotiated_features.contains(Features::RING_EVENT_IDX),
        );
        let recv_queue = match recv_queue {
            Ok(queue) => queue,
            Err(error) => {
                Self::stop_transport(&mut transport);
                return Err(error);
            }
        };

        transport.set_status(transport.get_status() | crate::transport::DeviceStatus::DRIVER_OK);

        Ok(VirtIONetRaw {
            transport,
            mac,
            header_layout,
            guest_csum: negotiated_features.contains(Features::GUEST_CSUM),
            rx_offloads,
            stopped: false,
            recv_queue,
            send_queue,
        })
    }

    // Reset acknowledgement is the DMA retirement barrier. Like the kernel
    // transport and Linux removal path, teardown must not release memory until
    // the device confirms it has stopped, even if a device fails to respond.
    fn stop_transport(transport: &mut T) {
        use crate::transport::DeviceStatus;
        if !transport.get_status().is_empty() {
            transport.set_status(DeviceStatus::empty());
            while !transport.get_status().is_empty() {
                core::hint::spin_loop();
            }
        }
    }

    pub(crate) fn stop_dma(&mut self) {
        if !self.stopped {
            Self::stop_transport(&mut self.transport);
            self.stopped = true;
        }
    }

    fn ensure_active(&self) -> Result<()> {
        if self.stopped {
            Err(Error::NotReady)
        } else {
            Ok(())
        }
    }

    /// Acknowledge interrupt.
    pub fn ack_interrupt(&mut self) -> bool {
        self.transport.ack_interrupt()
    }

    /// Disable interrupts.
    pub fn disable_interrupts(&mut self) {
        self.send_queue.set_dev_notify(false);
        self.recv_queue.set_dev_notify(false);
    }

    /// Enable interrupts.
    pub fn enable_interrupts(&mut self) {
        let _ = self.enable_interrupts_prepare();
    }

    /// Enable interrupts and capture the queue positions used for the
    /// subsequent race-closing check.
    pub fn enable_interrupts_prepare(&mut self) -> InterruptState {
        InterruptState {
            receive_used_idx: self.recv_queue.enable_dev_notify_prepare(),
            transmit_used_idx: self.send_queue.enable_dev_notify_prepare(),
        }
    }

    /// Return whether either queue gained a used buffer while interrupts were
    /// being enabled.
    ///
    /// This method includes the full memory barrier required between publishing
    /// the notification request and rereading the device-owned used indices.
    pub fn interrupt_pending(&self, state: InterruptState) -> bool {
        self.recv_queue.dev_notify_pending(state.receive_used_idx)
            || self.send_queue.dev_notify_pending(state.transmit_used_idx)
    }

    /// Get MAC address.
    pub fn mac_address(&self) -> EthernetAddress {
        self.mac
    }

    /// Whether can send packet.
    pub fn can_send(&self) -> bool {
        !self.stopped && self.send_queue.available_desc() >= 2
    }

    /// Whether the length of the receive buffer is valid.
    fn check_rx_buf_len(rx_buf: &[u8]) -> Result<()> {
        if rx_buf.len() < MIN_BUFFER_LEN {
            warn!("Receive buffer len {} is too small", rx_buf.len());
            Err(Error::InvalidParam)
        } else {
            Ok(())
        }
    }

    /// Whether the length of the transmit buffer is valid.
    fn check_tx_buf_len(&self, tx_buf: &[u8]) -> Result<()> {
        if tx_buf.len() < self.header_layout.len() {
            warn!("Transmit buffer len {} is too small", tx_buf.len());
            Err(Error::InvalidParam)
        } else {
            Ok(())
        }
    }

    /// Fill the header of the `buffer` with the common [`super::VirtioNetHdr`] prefix.
    ///
    /// If the `buffer` is not large enough, it returns [`Error::InvalidParam`].
    pub fn fill_buffer_header(&self, buffer: &mut [u8]) -> Result<usize> {
        self.header_layout.write(buffer)
    }

    /// Submits a request to transmit a buffer immediately without waiting for
    /// the transmission to complete.
    ///
    /// It will submit request to the VirtIO net device and return a token
    /// identifying the position of the first descriptor in the chain. If there
    /// are not enough descriptors to allocate, then it returns
    /// [`Error::QueueFull`].
    ///
    /// The caller needs to fill the `tx_buf` with a header by calling
    /// [`fill_buffer_header`] before transmission. Then it calls [`poll_transmit`]
    /// with the returned token to check whether the device has finished handling
    /// the request. Once it has, the caller must call [`transmit_complete`] with
    /// the same buffer before reading the result (transmitted length).
    ///
    /// # Safety
    ///
    /// `tx_buf` is still borrowed by the underlying VirtIO net device even after
    /// this method returns. Thus, it is the caller's responsibility to guarantee
    /// that they are not accessed before the request is completed in order to
    /// avoid data races.
    ///
    /// [`fill_buffer_header`]: Self::fill_buffer_header
    /// [`poll_transmit`]: Self::poll_transmit
    /// [`transmit_complete`]: Self::transmit_complete
    pub unsafe fn transmit_begin(&mut self, tx_buf: &[u8]) -> Result<u16> {
        self.ensure_active()?;
        self.check_tx_buf_len(tx_buf)?;
        let token = self.send_queue.add(&[tx_buf], &mut [])?;
        if self.send_queue.should_notify() {
            self.transport.notify(QUEUE_TRANSMIT);
        }
        Ok(token)
    }

    /// Fetches the token of the next completed transmission request from the
    /// used ring and returns it, without removing it from the used ring. If
    /// there are no pending completed requests it returns [`None`].
    pub fn poll_transmit(&mut self) -> Option<u16> {
        if self.stopped {
            None
        } else {
            self.send_queue.peek_used()
        }
    }

    /// Like [`Self::poll_transmit`], but rejects a device-provided descriptor
    /// ID which is outside this queue instead of narrowing it.
    pub fn poll_transmit_checked(&self) -> Result<Option<u16>> {
        self.ensure_active()?;
        Self::checked_used_token(self.send_queue.peek_used_id())
    }

    /// Completes a transmission operation which was started by [`transmit_begin`].
    /// Returns number of bytes transmitted.
    ///
    /// # Safety
    ///
    /// The same buffer must be passed in again as was passed to
    /// [`transmit_begin`] when it returned the token.
    ///
    /// [`transmit_begin`]: Self::transmit_begin
    pub unsafe fn transmit_complete(&mut self, token: u16, tx_buf: &[u8]) -> Result<usize> {
        let len = self.send_queue.pop_used(token, &[tx_buf], &mut [])?;
        Ok(len as usize)
    }

    /// Submits a request to receive a buffer immediately without waiting for
    /// the reception to complete.
    ///
    /// It will submit request to the VirtIO net device and return a token
    /// identifying the position of the first descriptor in the chain. If there
    /// are not enough descriptors to allocate, then it returns
    /// [`Error::QueueFull`].
    ///
    /// The caller can then call [`poll_receive`] with the returned token to
    /// check whether the device has finished handling the request. Once it has,
    /// the caller must call [`receive_complete`] with the same buffer before
    /// reading the response.
    ///
    /// # Safety
    ///
    /// `rx_buf` is still borrowed by the underlying VirtIO net device even after
    /// this method returns. Thus, it is the caller's responsibility to guarantee
    /// that they are not accessed before the request is completed in order to
    /// avoid data races.
    ///
    /// [`poll_receive`]: Self::poll_receive
    /// [`receive_complete`]: Self::receive_complete
    pub unsafe fn receive_begin(&mut self, rx_buf: &mut [u8]) -> Result<u16> {
        self.ensure_active()?;
        Self::check_rx_buf_len(rx_buf)?;
        if self
            .rx_offloads
            .intersects(RxOffloads::TCPV4 | RxOffloads::TCPV6)
            && rx_buf.len() < MIN_GSO_RX_BUFFER_LEN
        {
            return Err(Error::InvalidParam);
        }
        let token = self.recv_queue.add(&[], &mut [rx_buf])?;
        if self.recv_queue.should_notify() {
            self.transport.notify(QUEUE_RECEIVE);
        }
        Ok(token)
    }

    /// Fetches the token of the next completed reception request from the
    /// used ring and returns it, without removing it from the used ring. If
    /// there are no pending completed requests it returns [`None`].
    pub fn poll_receive(&self) -> Option<u16> {
        if self.stopped {
            None
        } else {
            self.recv_queue.peek_used()
        }
    }

    /// Like [`Self::poll_receive`], but rejects a device-provided descriptor ID
    /// which is outside this queue instead of narrowing it.
    pub fn poll_receive_checked(&self) -> Result<Option<u16>> {
        self.ensure_active()?;
        Self::checked_used_token(self.recv_queue.peek_used_id())
    }

    fn checked_used_token(id: Option<u32>) -> Result<Option<u16>> {
        match id {
            None => Ok(None),
            Some(id) if id < QUEUE_SIZE as u32 => Ok(Some(id as u16)),
            Some(_) => Err(Error::IoError),
        }
    }

    /// Completes a transmission operation which was started by [`receive_begin`].
    ///
    /// After completion, the `rx_buf` will contain a header followed by the
    /// received packet. It returns the length of the header and the length of
    /// the packet.
    ///
    /// # Safety
    ///
    /// The same buffer must be passed in again as was passed to
    /// [`receive_begin`] when it returned the token.
    ///
    /// [`receive_begin`]: Self::receive_begin
    pub unsafe fn receive_complete(
        &mut self,
        token: u16,
        rx_buf: &mut [u8],
    ) -> Result<(usize, usize)> {
        let len = self.reclaim_receive(token, rx_buf)?;
        self.prepare_receive(rx_buf, len)
    }

    /// Reclaim a receive descriptor without parsing its contents.
    ///
    /// Success transfers the buffer back to the CPU even if the returned used
    /// length or subsequent metadata is invalid. A queue error does not retire
    /// the descriptor: preserve its buffer until completion or confirmed reset.
    ///
    /// # Safety
    /// The token and allocation must exactly match receive_begin.
    pub unsafe fn reclaim_receive(&mut self, token: u16, buffer: &mut [u8]) -> Result<usize> {
        self.ensure_active()?;
        self.recv_queue
            .pop_used(token, &[], &mut [buffer])
            .map(|len| len as usize)
    }

    /// Copy negotiated parsing state for use outside the driver's lock.
    pub fn receive_parser(&self) -> ReceiveParser {
        ReceiveParser {
            layout: self.header_layout,
            guest_csum: self.guest_csum,
            offloads: self.rx_offloads,
        }
    }

    /// Complete an opt-in receive, retaining any negotiated GSO metadata.
    /// Consumers needing to distinguish queue errors from invalid CPU-owned
    /// packet data should call reclaim_receive and ReceiveParser::prepare in
    /// separate steps instead.
    ///
    /// # Safety
    /// The token and allocation must exactly match receive_begin.
    pub unsafe fn receive_complete_with_metadata(
        &mut self,
        token: u16,
        buffer: &mut [u8],
    ) -> Result<(usize, usize, ReceiveMetadata)> {
        let used = self.reclaim_receive(token, buffer)?;
        self.receive_parser().prepare(buffer, used)
    }

    pub(crate) fn prepare_receive(&self, buffer: &mut [u8], used: usize) -> Result<(usize, usize)> {
        self.header_layout.receive(buffer, used, self.guest_csum)
    }

    // Buffered owners and blocking operations use this after the reset barrier.
    // Raw callers own their outstanding payload allocations and must retain
    // them until completion or device teardown.
    pub(crate) unsafe fn detach_receive(&mut self, token: u16, buffer: &mut [u8]) -> Result<()> {
        debug_assert!(self.stopped);
        self.recv_queue.detach_unused(token, &[], &mut [buffer])
    }

    /// Sends a packet to the network, and blocks until the request completed.
    pub fn send(&mut self, tx_buf: &[u8]) -> Result {
        self.ensure_active()?;
        let mut storage = [0u8; MODERN_NET_HDR_SIZE];
        let len = self.header_layout.write(&mut storage)?;
        let header = &storage[..len];
        let inputs: &[&[u8]] = if tx_buf.is_empty() {
            &[header]
        } else {
            &[header, tx_buf]
        };
        // Keep both the stack header and caller payload alive until completion
        // or confirmed reset, including malformed completion IDs.
        let token = unsafe { self.send_queue.add(inputs, &mut [])? };
        if self.send_queue.should_notify() {
            self.transport.notify(QUEUE_TRANSMIT);
        }
        while self.send_queue.peek_used_id().is_none() {
            core::hint::spin_loop();
        }
        let result = unsafe { self.send_queue.pop_used(token, inputs, &mut []) };
        if result.is_err() {
            self.stop_dma();
            // No successful pop occurred, so the chain is still outstanding.
            unsafe { self.send_queue.detach_unused(token, inputs, &mut [])? };
        }
        result.map(|_| ())
    }

    /// Blocks and waits for a packet to be received.
    ///
    /// After completion, the `rx_buf` will contain a header followed by the
    /// received packet. It returns the length of the header and the length of
    /// the packet.
    pub fn receive_wait(&mut self, rx_buf: &mut [u8]) -> Result<(usize, usize)> {
        let token = unsafe { self.receive_begin(rx_buf)? };
        while self.recv_queue.peek_used_id().is_none() {
            core::hint::spin_loop();
        }
        let used = match unsafe { self.reclaim_receive(token, rx_buf) } {
            Ok(used) => used,
            Err(error) => {
                self.stop_dma();
                unsafe { self.detach_receive(token, rx_buf)? };
                return Err(error);
            }
        };
        let result = self.prepare_receive(rx_buf, used);
        if result.is_err() {
            self.stop_dma();
        }
        result
    }
}

impl<H: Hal, T: Transport, const QUEUE_SIZE: usize> Drop for VirtIONetRaw<H, T, QUEUE_SIZE> {
    fn drop(&mut self) {
        // Clear any pointers pointing to DMA regions, so the device doesn't try to access them
        // after they have been freed.
        self.stop_dma();
        self.transport.queue_unset(QUEUE_RECEIVE);
        self.transport.queue_unset(QUEUE_TRANSMIT);
    }
}
