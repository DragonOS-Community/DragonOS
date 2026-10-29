//! Seed a standalone device without registering a driver or accessing hardware.
//! Exercise the real inode methods, including consumption before user faults.
use super::*;
use crate::{
    filesystem::vfs::UserReadMode, libs::mutex::Mutex, mm::VirtAddr,
    syscall::user_buffer::UserBuffer,
};

const PACKETS: [u8; 6] = [0x08, 0x11, 0x22, 0x09, 0x33, 0x44];

fn seeded() -> Ps2MouseDevice {
    let device = Ps2MouseDevice::new();
    {
        let mut inner = device.inner.lock_irqsave();
        for byte in PACKETS {
            inner.buf.push(byte);
        }
    }
    device
}

fn short_reads() -> bool {
    let device = seeded();
    let data = Mutex::new(FilePrivateData::default());
    let mut zero = [0xa5; 3];
    if !matches!(device.read_at(0, 0, &mut zero, data.lock()), Ok(0))
        || zero != [0xa5; 3]
        || device.inner.lock_irqsave().buf.len() != PACKETS.len()
    {
        return false;
    }
    let mut bytes = [0u8; 6];
    // First bound by the supplied count, then by the supplied slice. Together
    // these leave two pending packet bytes which must remain immediately read.
    if !matches!(device.read_at(0, 1, &mut bytes[..3], data.lock()), Ok(1))
        || bytes[0] != PACKETS[0]
        || !matches!(device.read_at(0, 3, &mut bytes[1..2], data.lock()), Ok(1))
        || bytes[1] != PACKETS[1]
        || !matches!(device.read_at(0, 1, &mut bytes[2..3], data.lock()), Ok(1))
        || bytes[2] != PACKETS[2]
        || !matches!(device.read_at(0, 3, &mut bytes[3..], data.lock()), Ok(3))
    {
        return false;
    }
    bytes == PACKETS && device.inner.lock_irqsave().buf.is_empty()
}

fn fault_consumes_selected_packet() -> bool {
    let device = seeded();
    let data = Mutex::new(FilePrivateData::default());
    // The numeric address is admitted without constructing a Rust user slice.
    // Copying to the unmapped null page is protected by the exception table.
    let mut output = unsafe { UserBuffer::new(VirtAddr::new(0), 3) };
    if device.read_user_at(0, 3, &mut output, data.lock()) != Err(SystemError::EFAULT)
        || device.inner.lock_irqsave().buf.len() != 3
    {
        return false;
    }
    let mut remaining = [0u8; 3];
    matches!(device.read_at(0, 3, &mut remaining, data.lock()), Ok(3))
        && remaining == PACKETS[3..]
        && device.inner.lock_irqsave().buf.is_empty()
}

pub(crate) fn run() -> bool {
    let device = seeded();
    let bounded = short_reads();
    let fault = fault_consumes_selected_packet();
    let two_byte = seeded();
    let data = Mutex::new(FilePrivateData::default());
    let mut pair = [0u8; 2];
    let mut tail = [0u8; 1];
    let two_byte_short = matches!(two_byte.read_at(0, 2, &mut pair, data.lock()), Ok(2))
        && pair == PACKETS[..2]
        && matches!(two_byte.read_at(0, 1, &mut tail, data.lock()), Ok(1))
        && tail[0] == PACKETS[2];
    bounded
        && fault
        && two_byte_short
        && device.user_read_mode() == UserReadMode::Scalar
        && device.is_stream()
}
