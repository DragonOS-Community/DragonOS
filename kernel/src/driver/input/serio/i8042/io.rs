use crate::arch::{io::PortIOArch, CurrentPortIOArch};

const I8042_DATA_REG: u16 = 0x60;
const I8042_STATUS_REG: u16 = 0x64;
const I8042_COMMAND_REG: u16 = 0x64;

pub(super) fn read_status() -> u8 {
    return unsafe { CurrentPortIOArch::in8(I8042_STATUS_REG) };
}

pub(super) fn read_data() -> u8 {
    return unsafe { CurrentPortIOArch::in8(I8042_DATA_REG) };
}

pub(super) fn write_command(val: i32) {
    return unsafe { CurrentPortIOArch::out8(I8042_COMMAND_REG, val as u8) };
}

pub(super) fn write_data(val: i32) {
    return unsafe { CurrentPortIOArch::out8(I8042_DATA_REG, val as u8) };
}
