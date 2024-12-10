use core::ptr::NonNull;

use alloc::sync::Arc;
use fdt::node::FdtNode;
use log::info;
use system_error::SystemError;
use virtio_drivers::transport::{
    mmio::{MmioTransport, VirtIOHeader},
    Transport,
};

use crate::{
    arch::MMArch,
    driver::base::device::DeviceId,
    exception::HardwareIrqNumber,
    libs::align::page_align_up,
    mm::{
        mmio_buddy::{mmio_pool, MMIOSpaceGuard},
        MemoryManagementArch, PhysAddr,
    },
};

pub struct VirtIOMmioTransport {
    mmio_transport: MmioTransport,
    _mmio_guard: MMIOSpaceGuard,
    irq: HardwareIrqNumber,
    device_id: Arc<DeviceId>,
}

impl VirtIOMmioTransport {
    pub fn new(node: FdtNode) -> Result<Self, SystemError> {
        let reg = node
            .reg()
            .ok_or(SystemError::EINVAL)?
            .next()
            .ok_or(SystemError::EINVAL)?;
        let paddr = reg.starting_address as usize;
        let size = reg.size.unwrap_or(0);
        let page_offset = paddr % MMArch::PAGE_SIZE;
        let paddr = paddr - page_offset;
        let size = page_align_up(size + page_offset);
        let irq = node
            .interrupts()
            .ok_or(SystemError::EINVAL)?
            .next()
            .ok_or(SystemError::EINVAL)?;

        let device_id = DeviceId::new(None, Some(format!("virtio_mmio_{:#X}", paddr))).unwrap();

        let mmio_guard = mmio_pool().create_mmio(size)?;
        unsafe { mmio_guard.map_phys(PhysAddr::new(paddr), size) }?;

        let vaddr = mmio_guard.vaddr() + page_offset;
        let header = NonNull::new(vaddr.data() as *mut VirtIOHeader).unwrap();

        match unsafe { MmioTransport::new(header) } {
            Ok(mmio_transport) => {
                info!( "Detected virtio MMIO device with vendor id {:#X}, device type {:?}, version {:?}, hw irq: {}",
                    mmio_transport.vendor_id(),
                    mmio_transport.device_type(),
                    mmio_transport.version(),
                    irq as u32
                );

                Ok(Self {
                    mmio_transport,
                    _mmio_guard: mmio_guard,
                    irq: HardwareIrqNumber::new(irq as u32),
                    device_id,
                })
            }
            Err(_) => {
                // warn!("MmioTransport::new failed: {:?}", e);
                Err(SystemError::EINVAL)
            }
        }
    }

    #[allow(dead_code)]
    #[inline]
    pub fn irq(&self) -> HardwareIrqNumber {
        self.irq
    }

    pub fn device_id(&self) -> Arc<DeviceId> {
        self.device_id.clone()
    }
}

impl Transport for VirtIOMmioTransport {
    fn device_type(&self) -> virtio_drivers::transport::DeviceType {
        self.mmio_transport.device_type()
    }

    fn read_device_features(&mut self) -> u64 {
        self.mmio_transport.read_device_features()
    }

    fn write_driver_features(&mut self, driver_features: u64) {
        self.mmio_transport.write_driver_features(driver_features)
    }

    fn max_queue_size(&mut self, queue: u16) -> u32 {
        self.mmio_transport.max_queue_size(queue)
    }

    fn notify(&mut self, queue: u16) {
        self.mmio_transport.notify(queue)
    }

    fn get_status(&self) -> virtio_drivers::transport::DeviceStatus {
        self.mmio_transport.get_status()
    }

    fn set_status(&mut self, status: virtio_drivers::transport::DeviceStatus) {
        self.mmio_transport.set_status(status)
    }

    fn set_guest_page_size(&mut self, guest_page_size: u32) {
        self.mmio_transport.set_guest_page_size(guest_page_size)
    }

    fn requires_legacy_layout(&self) -> bool {
        self.mmio_transport.requires_legacy_layout()
    }

    fn queue_set(
        &mut self,
        queue: u16,
        size: u32,
        descriptors: virtio_drivers::PhysAddr,
        driver_area: virtio_drivers::PhysAddr,
        device_area: virtio_drivers::PhysAddr,
    ) {
        self.mmio_transport
            .queue_set(queue, size, descriptors, driver_area, device_area)
    }

    fn queue_unset(&mut self, queue: u16) {
        self.mmio_transport.queue_unset(queue)
    }

    fn queue_used(&mut self, queue: u16) -> bool {
        self.mmio_transport.queue_used(queue)
    }

    fn ack_interrupt(&mut self) -> bool {
        self.mmio_transport.ack_interrupt()
    }

    fn config_space<T: 'static>(&self) -> virtio_drivers::Result<core::ptr::NonNull<T>> {
        self.mmio_transport.config_space()
    }

    fn finish_init(&mut self) {
        self.mmio_transport.finish_init()
    }
}
