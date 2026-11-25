use super::EthernetAddress;
use crate::hal::{BufferDirection, Dma, Hal};
use crate::queue::VirtQueue;
use crate::transport::Transport;
use crate::{Error, Result, PAGE_SIZE};

/// The receive-filter operations negotiated by VIRTIO_NET_F_CTRL_RX.
#[derive(Debug)]
pub enum RxFilterCommand<'a> {
    /// Enable or disable acceptance of frames for other destination MACs.
    Promiscuous(bool),
    /// Enable or disable acceptance of all multicast destination MACs.
    AllMulticast(bool),
    /// Replace the additional unicast and multicast MAC filters.
    MacTable {
        /// Additional unicast destinations.
        unicast: &'a [EthernetAddress],
        /// Additional multicast destinations.
        multicast: &'a [EthernetAddress],
    },
}

// A single RX/TX queue pair occupies indices zero and one.
pub(super) const QUEUE_CONTROL: u16 = 2;
const QUEUE_SIZE: usize = 2;

pub(super) struct ControlQueue<H: Hal> {
    queue: VirtQueue<H, QUEUE_SIZE>,
    buffer: Option<Dma<H>>,
    pending: Option<(u16, usize)>,
}

impl<H: Hal> ControlQueue<H> {
    pub(super) fn new(
        transport: &mut impl Transport,
        event_idx: bool,
        rx_filter: bool,
    ) -> Result<Self> {
        let buffer = if rx_filter {
            Some(Dma::new(1, BufferDirection::Both)?)
        } else {
            None
        };
        let mut queue = VirtQueue::new(transport, QUEUE_CONTROL, false, event_idx)?;
        queue.set_dev_notify(false);
        Ok(Self {
            queue,
            buffer,
            pending: None,
        })
    }

    pub(super) fn supports_rx_filter(&self) -> bool {
        self.buffer.is_some()
    }

    pub(super) fn start(
        &mut self,
        transport: &mut impl Transport,
        command: RxFilterCommand<'_>,
    ) -> Result<()> {
        if self.pending.is_some() {
            return Err(Error::AlreadyUsed);
        }
        let buffer = self.buffer.as_mut().ok_or(Error::Unsupported)?;
        // The CPU owns the entire page until the descriptor chain is submitted.
        let buffer = unsafe { buffer.raw_slice().as_mut() };
        let length = match command {
            RxFilterCommand::Promiscuous(enable) => {
                buffer[..3].copy_from_slice(&[0, 0, u8::from(enable)]);
                3
            }
            RxFilterCommand::AllMulticast(enable) => {
                buffer[..3].copy_from_slice(&[0, 1, u8::from(enable)]);
                3
            }
            RxFilterCommand::MacTable { unicast, multicast } => {
                const HEADER_LEN: usize = 10;
                let count = unicast
                    .len()
                    .checked_add(multicast.len())
                    .filter(|count| *count <= (PAGE_SIZE - HEADER_LEN - 1) / 6)
                    .ok_or(Error::InvalidParam)?;
                buffer[..2].copy_from_slice(&[1, 0]);
                let middle = 6 + unicast.len() * 6;
                let length = HEADER_LEN + count * 6;
                encode_mac_list(&mut buffer[2..middle], unicast);
                encode_mac_list(&mut buffer[middle..length], multicast);
                length
            }
        };
        let (request, status) = buffer.split_at_mut(length);
        status[0] = u8::MAX;
        // The page stays at the same address, exclusively assigned to this
        // command until completion or confirmed reset.
        let token = unsafe { self.queue.add(&[request], &mut [&mut status[..1]])? };
        self.pending = Some((token, length));
        if self.queue.should_notify() {
            transport.notify(QUEUE_CONTROL);
        }
        Ok(())
    }

    pub(super) fn poll(&mut self) -> Result<Option<bool>> {
        let (token, length) = self.pending.ok_or(Error::NotReady)?;
        if !self.queue.can_pop() {
            return Ok(None);
        }
        let buffer = self.buffer.as_mut().expect("pending command owns DMA");
        // The queue validates the completion before unsharing these buffers.
        // Access the status byte only after the expected chain is reclaimed.
        let buffer = unsafe { buffer.raw_slice().as_mut() };
        let (request, status) = buffer.split_at_mut(length);
        let written = unsafe {
            self.queue
                .pop_used(token, &[request], &mut [&mut status[..1]])?
        };
        self.pending = None;
        if written != 1 {
            return Err(Error::IoError);
        }
        match status[0] {
            0 => Ok(Some(true)),
            1 => Ok(Some(false)),
            _ => Err(Error::IoError),
        }
    }

    /// # Safety
    /// The transport has confirmed that device DMA is stopped.
    pub(super) unsafe fn detach_after_reset(&mut self) {
        if let Some((token, length)) = self.pending.take() {
            let buffer = self.buffer.as_mut().expect("pending command owns DMA");
            let buffer = unsafe { buffer.raw_slice().as_mut() };
            let (request, status) = buffer.split_at_mut(length);
            // The token and both slices come from the original submission.
            unsafe {
                self.queue
                    .detach_unused(token, &[request], &mut [&mut status[..1]])
                    .expect("pending control token is valid");
            }
        }
    }
}

fn encode_mac_list(bytes: &mut [u8], addresses: &[EthernetAddress]) {
    bytes[..4].copy_from_slice(&(addresses.len() as u32).to_le_bytes());
    for (entry, address) in bytes[4..].chunks_exact_mut(6).zip(addresses) {
        entry.copy_from_slice(address);
    }
}
