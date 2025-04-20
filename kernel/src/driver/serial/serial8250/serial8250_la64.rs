use system_error::SystemError;

use crate::{
    arch::MMArch,
    libs::spinlock::SpinLock,
    mm::{MemoryManagementArch, PhysAddr, VirtAddr},
};

const UART_PADDR_COM1: PhysAddr = PhysAddr::new(0x01FE001E0);

static mut UART_PORT_COM1: Option<SpinLock<Serial8250LA64Port>> = None;

struct Serial8250LA64Port {
    base_address: VirtAddr,
}

impl Serial8250LA64Port {
    pub fn new(base_address: PhysAddr) -> Self {
        Self {
            base_address: unsafe { MMArch::phys_2_virt(base_address).unwrap() },
        }
    }

    pub fn putchar(&mut self, c: u8) {
        let ptr = self.base_address.as_ptr() as *mut u8;
        loop {
            unsafe {
                if ptr.add(5).read_volatile() & (1 << 5) != 0 {
                    break;
                }
            }
        }
        unsafe {
            ptr.add(0).write_volatile(c);
        }
    }

    pub fn getchar(&mut self) -> Option<u8> {
        let ptr = self.base_address.as_ptr() as *mut u8;
        unsafe {
            if ptr.add(5).read_volatile() & 1 == 0 {
                // The DR bit is 0, meaning no data
                None
            } else {
                // The DR bit is 1, meaning data!
                Some(ptr.add(0).read_volatile())
            }
        }
    }
}

#[inline(never)]
pub(super) fn early_la64_seria8250_init() -> Result<(), SystemError> {
    let port = Serial8250LA64Port::new(UART_PADDR_COM1);
    unsafe {
        UART_PORT_COM1 = Some(SpinLock::new(port));
    }
    send_to_default_serial8250_la64_port(b"[DragonOS] loongarch64 debug uart port initialized!\n");
    Ok(())
}

pub(super) fn send_to_default_serial8250_la64_port(s: &[u8]) {
    if let Some(com) = unsafe { UART_PORT_COM1.as_ref() } {
        let mut cg = com.lock_irqsave();
        for c in s.iter() {
            cg.putchar(*c);
        }
    }
}
