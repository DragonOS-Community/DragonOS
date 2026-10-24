use alloc::vec;

use super::net_buf::{RxBuffer, TxBuffer};
use super::{EthernetAddress, VirtIONetRaw};
use crate::{hal::Hal, transport::Transport, Error, Result};

/// Driver for a VirtIO network device.
///
/// Unlike [`VirtIONetRaw`], it uses [`RxBuffer`]s for transmission and
/// reception rather than the raw slices. On initialization, it pre-allocates
/// all receive buffers and puts them all in the receive queue.
///
/// The virtio network device is a virtual ethernet card.
///
/// It has enhanced rapidly and demonstrates clearly how support for new
/// features are added to an existing device.
/// Empty buffers are placed in one virtqueue for receiving packets, and
/// outgoing packets are enqueued into another for transmission in that order.
/// A third command queue is used to control advanced filtering features.
pub struct VirtIONet<H: Hal, T: Transport, const QUEUE_SIZE: usize> {
    inner: VirtIONetRaw<H, T, QUEUE_SIZE>,
    rx_buffers: [Option<RxBuffer>; QUEUE_SIZE],
}

impl<H: Hal, T: Transport, const QUEUE_SIZE: usize> VirtIONet<H, T, QUEUE_SIZE> {
    /// Create a new VirtIO-Net driver.
    pub fn new(transport: T, buf_len: usize) -> Result<Self> {
        let inner = VirtIONetRaw::new(transport)?;
        const NONE_BUF: Option<RxBuffer> = None;
        // Establish the owner before publishing the first payload descriptor.
        // Its Drop stops DMA before any of these buffers can be freed.
        let mut device = Self {
            inner,
            rx_buffers: [NONE_BUF; QUEUE_SIZE],
        };
        for i in 0..QUEUE_SIZE {
            let mut rx_buf = RxBuffer::new(i, buf_len);
            // SAFETY: the allocation is retained in device until completion or reset.
            let token = unsafe { device.inner.receive_begin(rx_buf.as_bytes_mut())? };
            if token as usize >= QUEUE_SIZE || device.rx_buffers[token as usize].is_some() {
                device.inner.stop_dma();
                return Err(Error::WrongToken);
            }
            rx_buf.idx = token;
            device.rx_buffers[token as usize] = Some(rx_buf);
        }
        Ok(device)
    }

    /// Acknowledge interrupt.
    pub fn ack_interrupt(&mut self) -> bool {
        self.inner.ack_interrupt()
    }

    /// Disable interrupts.
    pub fn disable_interrupts(&mut self) {
        self.inner.disable_interrupts()
    }

    /// Enable interrupts.
    pub fn enable_interrupts(&mut self) {
        self.inner.enable_interrupts()
    }

    /// Get MAC address.
    pub fn mac_address(&self) -> EthernetAddress {
        self.inner.mac_address()
    }

    /// Whether can send packet.
    pub fn can_send(&self) -> bool {
        self.inner.can_send()
    }

    /// Whether can receive packet.
    pub fn can_recv(&self) -> bool {
        self.inner.poll_receive().is_some()
    }

    /// Receives a [`RxBuffer`] from network. If currently no data, returns an
    /// error with type [`Error::NotReady`].
    ///
    /// It will try to pop a buffer that completed data reception in the
    /// NIC queue.
    pub fn receive(&mut self) -> Result<RxBuffer> {
        let token = match self.inner.poll_receive_checked() {
            Ok(Some(token)) => token,
            Ok(None) => return Err(Error::NotReady),
            Err(error) => {
                self.inner.stop_dma();
                return Err(error);
            }
        };
        let result = match self.rx_buffers[token as usize].as_mut() {
            Some(buffer) if buffer.idx == token => {
                // SAFETY: the same allocation remains owned by this slot.
                unsafe { self.inner.reclaim_receive(token, buffer.as_bytes_mut()) }
            }
            _ => Err(Error::WrongToken),
        };
        let used = match result {
            Ok(lengths) => lengths,
            Err(error) => {
                // Reclamation failed: retain the slot until reset and detach
                // in Drop, rather than releasing device-owned memory.
                self.inner.stop_dma();
                return Err(error);
            }
        };
        // Successful reclamation retires this slot even if header validation fails.
        let mut buffer = self.rx_buffers[token as usize].take().unwrap();
        let (header_len, packet_len) = match self.inner.prepare_receive(buffer.as_bytes_mut(), used)
        {
            Ok(lengths) => lengths,
            Err(error) => {
                self.inner.stop_dma();
                return Err(error);
            }
        };
        buffer.header_len = header_len;
        buffer.set_packet_len(packet_len);
        Ok(buffer)
    }

    /// Gives back the ownership of `rx_buf`, and recycles it for next use.
    ///
    /// It will add the buffer back to the NIC queue.
    pub fn recycle_rx_buffer(&mut self, mut rx_buf: RxBuffer) -> Result {
        // Safe because we take the ownership of `rx_buf` back to `rx_buffers`,
        // it lives as long as the queue.
        let new_token = unsafe { self.inner.receive_begin(rx_buf.as_bytes_mut()) }?;
        // `rx_buffers[new_token]` is expected to be `None` since it was taken
        // away at `Self::receive()` and has not been added back.
        if new_token as usize >= QUEUE_SIZE || self.rx_buffers[new_token as usize].is_some() {
            self.inner.stop_dma();
            return Err(Error::WrongToken);
        }
        rx_buf.idx = new_token;
        self.rx_buffers[new_token as usize] = Some(rx_buf);
        Ok(())
    }

    /// Allocate a new buffer for transmitting.
    pub fn new_tx_buffer(&self, buf_len: usize) -> TxBuffer {
        TxBuffer(vec![0; buf_len])
    }

    /// Sends a [`TxBuffer`] to the network, and blocks until the request
    /// completed.
    pub fn send(&mut self, tx_buf: TxBuffer) -> Result {
        self.inner.send(tx_buf.packet())
    }
}

impl<H: Hal, T: Transport, const QUEUE_SIZE: usize> Drop for VirtIONet<H, T, QUEUE_SIZE> {
    fn drop(&mut self) {
        self.inner.stop_dma();
        for buffer in self.rx_buffers.iter_mut().flatten() {
            // SAFETY: reset is confirmed, and slots contain only outstanding
            // descriptors; successfully reclaimed buffers were removed.
            unsafe { self.inner.detach_receive(buffer.idx, buffer.as_bytes_mut()) }
                .expect("buffered receive slot must own its descriptor");
        }
    }
}
