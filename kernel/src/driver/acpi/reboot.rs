use core::{hint::spin_loop, ptr, slice};

use acpi::{
    address::{AccessSize, AddressSpace, GenericAddress},
    fadt::Fadt,
    AcpiTable,
};
use alloc::vec::Vec;
use log::{debug, error, warn};
use system_error::SystemError;

use crate::{
    arch::{io::PortIOArch, CurrentPortIOArch},
    driver::pci::{
        pci::BusDeviceFunction,
        root::{pci_root_0, pci_root_manager},
    },
    mm::{early_ioremap::EarlyIoRemap, PhysAddr, VirtAddr},
    time::{sleep::nanosleep, PosixTimeSpec},
};

use super::acpi_manager;

const ACPI_ACCESS_BIT_SHIFT: u8 = 2;
const ACPI_PM1_SLEEP_TYPE_SHIFT: u16 = 10;
const ACPI_PM1_SLEEP_ENABLE: u16 = 1 << 13;
const ACPI_PM1_SLEEP_TYPE_MASK: u16 = 0x7 << ACPI_PM1_SLEEP_TYPE_SHIFT;
const ACPI_PM1_SLEEP_MASK: u16 = ACPI_PM1_SLEEP_TYPE_MASK | ACPI_PM1_SLEEP_ENABLE;
const ACPI_PM1_CONTROL_WRITEONLY_BITS: u16 = 0x2004;
const ACPI_SLEEP_CONTROL_TYPE_SHIFT: u16 = 2;
const ACPI_SLEEP_CONTROL_ENABLE: u16 = 1 << 5;
const ACPI_POWER_OFF_SPIN_LOOPS: usize = 10_000_000;
const ACPI_ENABLE_SPIN_LOOPS: usize = 1_000_000;

struct AcpiMap {
    physical_start: usize,
    virtual_start: usize,
    size: usize,
}

struct AcpiMapList {
    list: Vec<AcpiMap>,
}

impl AcpiMapList {
    fn new() -> AcpiMapList {
        AcpiMapList { list: Vec::new() }
    }

    fn find_vaddr(&self, paddr: usize, size: usize) -> Option<usize> {
        for map in self.list.iter() {
            if map.physical_start <= paddr && paddr + size <= map.physical_start + map.size {
                return Some(map.virtual_start + paddr - map.physical_start);
            }
        }
        return None;
    }
}

lazy_static! {
    /// RegisterMapList全局实例
    static ref ACPI_MAP_LIST: AcpiMapList = AcpiMapList::new();
}

pub fn acpi_reboot() {
    // 获取FADT表
    let fadt = acpi_manager()
        .tables()
        .unwrap()
        .find_table::<Fadt>()
        .expect("acpi_reboot(): failed to find Fadt table");

    // 获取ACPI重置寄存器信息
    let reset_register = fadt
        .reset_register()
        .expect("acpi_reboot(): failed to find reset register");

    // acpi重置寄存器是在fadt版本2及以上才引入的
    if fadt.header().revision < 2 {
        return;
    }

    // 检查FADT的标志位是否支持重置寄存器, 如果不支持直接返回
    let flags = fadt.flags;
    if !flags.supports_system_reset_via_fadt() {
        return;
    }

    // 获取要写入重置寄存器的值
    let reset_value = fadt.reset_value;

    // 根据地址空间类型执行重启操作，只可能存在IO，Memory和PCI配置空间
    let space_type = reset_register.address_space;
    debug!(
        "ACPI RESET_REG address space type: {:?}, reset value: {:?}\n",
        space_type, reset_value
    );
    match space_type {
        AddressSpace::PciConfigSpace => {
            acpi_pci_reboot(reset_register, reset_value);
        }
        AddressSpace::SystemMemory | AddressSpace::SystemIo => {
            debug!("ACPI Memory or I/O RESET_REG. \n");
            acpi_reset();
        }
        _ => {
            debug!("ACPI RESET_REG is not Memory, I/O or PCI. \n");
        }
    }

    // 重启命令下达后，并非所有平台都会立即响应，为了防止与后续的重启机制发生竞争，代码在写入重置寄存器后延时15ms，确保系统有足够的时间执行重启操作
    let sleep_time = PosixTimeSpec {
        tv_sec: 0,
        tv_nsec: 15_000_000, // 15ms
    };
    let _ = nanosleep(sleep_time);
}

pub fn acpi_power_off() -> Result<(), SystemError> {
    let tables = acpi_manager().tables().ok_or(SystemError::ENODEV)?;
    let fadt = tables
        .find_table::<Fadt>()
        .map_err(|_| SystemError::ENODEV)?;
    let (slp_typa, slp_typb) = find_s5_sleep_type().ok_or(SystemError::ENODEV)?;

    acpi_try_enable(&fadt);

    let pm1a_control = fadt.pm1a_control_block().map_err(|_| SystemError::ENODEV)?;
    if pm1a_control.address != 0 {
        let pm1b_control = fadt
            .pm1b_control_block()
            .map_err(|_| SystemError::ENODEV)?
            .filter(|register| register.address != 0);
        let current_control = acpi_read_pm1_control_pair(pm1a_control, pm1b_control)? as u16;
        let pm1_base = (current_control & !ACPI_PM1_CONTROL_WRITEONLY_BITS) & !ACPI_PM1_SLEEP_MASK;
        let pm1a_value = pm1_base | ((slp_typa & 0x7) << ACPI_PM1_SLEEP_TYPE_SHIFT);
        let pm1b_value = pm1_base | ((slp_typb & 0x7) << ACPI_PM1_SLEEP_TYPE_SHIFT);

        acpi_write_pm1_control_pair(pm1a_control, pm1a_value, pm1b_control, pm1b_value)?;
        acpi_write_pm1_control_pair(
            pm1a_control,
            pm1a_value | ACPI_PM1_SLEEP_ENABLE,
            pm1b_control,
            pm1b_value | ACPI_PM1_SLEEP_ENABLE,
        )?;
    } else {
        if let Some(sleep_control) = fadt
            .sleep_control_register()
            .map_err(|_| SystemError::ENODEV)?
            .filter(|register| register.address != 0)
        {
            let value = (((slp_typa & 0x7) << ACPI_SLEEP_CONTROL_TYPE_SHIFT)
                | ACPI_SLEEP_CONTROL_ENABLE) as u64;
            acpi_write_gas(sleep_control, value)?;
        }
    }

    for _ in 0..ACPI_POWER_OFF_SPIN_LOOPS {
        spin_loop();
    }

    Err(SystemError::EIO)
}

fn acpi_write_pm1_control_pair(
    pm1a_control: GenericAddress,
    pm1a_value: u16,
    pm1b_control: Option<GenericAddress>,
    pm1b_value: u16,
) -> Result<(), SystemError> {
    acpi_write_gas(pm1a_control, pm1a_value as u64)?;
    if let Some(pm1b_control) = pm1b_control {
        acpi_write_gas(pm1b_control, pm1b_value as u64)?;
    }

    Ok(())
}

fn acpi_read_pm1_control_pair(
    pm1a_control: GenericAddress,
    pm1b_control: Option<GenericAddress>,
) -> Result<u64, SystemError> {
    let mut value = acpi_read_gas(pm1a_control)?;
    if let Some(pm1b_control) = pm1b_control {
        value |= acpi_read_gas(pm1b_control)?;
    }

    Ok(value)
}

fn find_s5_sleep_type() -> Option<(u16, u16)> {
    let tables = acpi_manager().tables()?;
    let dsdt = tables.dsdt().ok()?;
    let (vaddr, _) =
        EarlyIoRemap::map(PhysAddr::new(dsdt.address), dsdt.length as usize, false).ok()?;
    let bytes = unsafe { slice::from_raw_parts(vaddr.data() as *const u8, dsdt.length as usize) };
    let result = parse_s5_package(bytes);
    let _ = EarlyIoRemap::unmap(vaddr);
    result
}

fn parse_s5_package(bytes: &[u8]) -> Option<(u16, u16)> {
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if &bytes[i..i + 4] == b"_S5_" {
            if i >= 1 && bytes[i - 1] == 0x08 {
                if let Some(result) = parse_s5_package_after_name(bytes, i + 4) {
                    return Some(result);
                }
            }
            if i >= 2 && bytes[i - 2] == 0x08 && bytes[i - 1] == b'\\' {
                if let Some(result) = parse_s5_package_after_name(bytes, i + 4) {
                    return Some(result);
                }
            }
        }
        i += 1;
    }

    None
}

fn parse_s5_package_after_name(bytes: &[u8], mut index: usize) -> Option<(u16, u16)> {
    let is_var_package = match bytes.get(index).copied()? {
        0x12 => false,
        0x13 => true,
        _ => return None,
    };
    index += 1;

    let (_package_end, next_index) = parse_aml_package_length(bytes, index)?;
    index = next_index;

    let (elements, next_index) = if is_var_package {
        parse_aml_integer(bytes, index)?
    } else {
        (*bytes.get(index)? as u64, index + 1)
    };
    if elements < 2 {
        return None;
    }
    index = next_index;

    let (slp_typa, next_index) = parse_aml_integer(bytes, index)?;
    let (slp_typb, _) = parse_aml_integer(bytes, next_index)?;
    Some((slp_typa as u16, slp_typb as u16))
}

fn parse_aml_package_length(bytes: &[u8], index: usize) -> Option<(usize, usize)> {
    let lead = *bytes.get(index)?;
    let byte_count = (lead >> 6) as usize;
    let mut length = if byte_count == 0 {
        (lead & 0x3f) as usize
    } else {
        (lead & 0x0f) as usize
    };
    let mut next_index = index + 1;

    for shift in 0..byte_count {
        length |= (*bytes.get(next_index)? as usize) << (4 + shift * 8);
        next_index += 1;
    }

    Some((index + length, next_index))
}

fn parse_aml_integer(bytes: &[u8], index: usize) -> Option<(u64, usize)> {
    match *bytes.get(index)? {
        0x00 => Some((0, index + 1)),
        0x01 => Some((1, index + 1)),
        0x0a => Some((*bytes.get(index + 1)? as u64, index + 2)),
        0x0b => Some((
            u16::from_le_bytes([*bytes.get(index + 1)?, *bytes.get(index + 2)?]) as u64,
            index + 3,
        )),
        0x0c => Some((
            u32::from_le_bytes([
                *bytes.get(index + 1)?,
                *bytes.get(index + 2)?,
                *bytes.get(index + 3)?,
                *bytes.get(index + 4)?,
            ]) as u64,
            index + 5,
        )),
        0x0e => Some((
            u64::from_le_bytes([
                *bytes.get(index + 1)?,
                *bytes.get(index + 2)?,
                *bytes.get(index + 3)?,
                *bytes.get(index + 4)?,
                *bytes.get(index + 5)?,
                *bytes.get(index + 6)?,
                *bytes.get(index + 7)?,
                *bytes.get(index + 8)?,
            ]),
            index + 9,
        )),
        _ => None,
    }
}

fn acpi_try_enable(fadt: &Fadt) {
    if fadt.smi_cmd_port == 0 || fadt.acpi_enable == 0 {
        return;
    }

    unsafe {
        CurrentPortIOArch::out8(fadt.smi_cmd_port as u16, fadt.acpi_enable);
    }

    for _ in 0..ACPI_ENABLE_SPIN_LOOPS {
        spin_loop();
    }
}

fn acpi_pci_reboot(reset_register: GenericAddress, reset_value: u8) {
    debug!("Acpi pci reboot");
    // 查找domain为0, bus为0的bus，重置寄存器只能存在于pci bus0上
    if !pci_root_manager().has_root(0) {
        return;
    }

    // 构造PCI设备和功能号
    let device = (((reset_register.address >> 32) & 0xffff & 0x1f) << 3) as u8;
    let function = ((reset_register.address >> 16) & 0xffff & 0x07) as u8;
    let devfn = BusDeviceFunction {
        bus: 0,
        device,
        function,
    };

    // 写入reset value
    debug!("Reseting with ACPI PCI RESET_REG.\n");
    pci_root_0().write_config(
        devfn,
        (reset_register.address & 0xffff) as u16,
        reset_value as u32,
    );
}

fn acpi_reset() {
    debug!("Acpi reset");
    // 获取FADT表
    let fadt = acpi_manager()
        .tables()
        .unwrap()
        .find_table::<Fadt>()
        .expect("acpi_reboot(): failed to find Fadt table");

    // 获取ACPI重置寄存器信息
    let reset_register = fadt
        .reset_register()
        .expect("acpi_reboot(): failed to find reset register");

    // 检查FADT的标志位是否支持重置寄存器, 如果不支持直接返回
    let flags = fadt.flags;
    if !flags.supports_system_reset_via_fadt() || reset_register.address == 0 {
        return;
    }

    // 获取要写入重置寄存器的值
    let reset_value = fadt.reset_value;

    // 如果重置寄存器是IO地址空间
    if reset_register.address_space == AddressSpace::SystemIo {
        debug!(
            "acpi reset register address: 0x{:x}, value: 0x{:x}\n",
            reset_register.address, reset_value
        );
        unsafe { CurrentPortIOArch::out8(reset_register.address as u16, reset_value) };
    } else {
        // 如果在内存空间，写入相应的内存空间
        let _ = acpi_write_gas(reset_register, reset_value as u64);
    }
}

/// # 向acpi寄存器写入数据
fn acpi_write_gas(register: GenericAddress, value: u64) -> Result<(), SystemError> {
    // 验证寄存器地址是否合法，并获取地址值
    let address = validate_register(register, 64)?;

    let access_width = get_access_bit_width(address, register, 64);
    // 计算总位宽
    let mut bit_width = register.bit_offset + register.bit_width;
    // 获取位偏移量
    let mut bit_offset = register.bit_offset;

    let mut index = 0;
    while bit_width != 0 {
        // 从value中提取需要写入的位段
        let value64 = get_bits(value, index * access_width, mask_bits_above(access_width));

        if bit_offset >= access_width {
            // 如果偏移量大于等于访问宽度，则向左调整偏移
            bit_offset -= access_width;
        } else {
            let access_address = address + (index * (access_width >> 3)) as u64;
            match register.address_space {
                AddressSpace::SystemMemory => {
                    write_memory(access_address as usize, value64, access_width as u32)?
                }
                AddressSpace::SystemIo => unsafe {
                    write_io(access_address as u16, value64 as u32, access_width as u32)?
                },
                _ => return Err(SystemError::ENOSYS),
            }
        }
        // 计算剩余的位宽度
        bit_width -= if bit_width > access_width {
            access_width
        } else {
            bit_width
        };
        index += 1;
    }

    debug!("ACPI HW Write: 0x{:x} to 0x{:x}\n", value, address);
    Ok(())
}

/// # 从acpi寄存器读取数据
fn acpi_read_gas(register: GenericAddress) -> Result<u64, SystemError> {
    let address = validate_register(register, 64)?;
    let access_width = get_access_bit_width(address, register, 64);
    let mut bit_width = register.bit_offset + register.bit_width;
    let mut bit_offset = register.bit_offset;
    let mut value = 0;

    let mut index = 0;
    while bit_width != 0 {
        let value64 = if bit_offset >= access_width {
            bit_offset -= access_width;
            0
        } else {
            let access_address = address + (index * (access_width >> 3)) as u64;
            match register.address_space {
                AddressSpace::SystemMemory => {
                    read_memory(access_address as usize, access_width as u32)?
                }
                AddressSpace::SystemIo => unsafe {
                    read_io(access_address as u16, access_width as u32)? as u64
                },
                _ => return Err(SystemError::ENOSYS),
            }
        };

        value |= (value64 & mask_bits_above(access_width)) << (index * access_width);
        bit_width -= if bit_width > access_width {
            access_width
        } else {
            bit_width
        };
        index += 1;
    }

    debug!("ACPI HW Read: 0x{:x} from 0x{:x}\n", value, address);
    Ok(value)
}

/// # 检查GAS寄存器是否有效，并确保其访问宽度和位宽在允许范围内
fn validate_register(register: GenericAddress, max_bit_width: u8) -> Result<u64, SystemError> {
    let address = register.address;
    if address == 0 {
        return Err(SystemError::EINVAL);
    }

    // 验证地址空间类型
    if register.address_space != AddressSpace::SystemIo
        && register.address_space != AddressSpace::SystemMemory
    {
        error!(
            "Unsupported address space type: {:?}\n",
            register.address_space
        );
        return Err(SystemError::ENOSYS);
    }

    // 验证bit width
    let access_width = get_access_bit_width(address, register, max_bit_width);
    let bit_width = round_up(register.bit_offset + register.bit_width, access_width);
    // 如果最大位宽小于实际寄存器位宽
    if max_bit_width < bit_width {
        warn!(
            "Requested bit width 0x{:x} is smaller than the register bit width 0x{:x}\n",
            max_bit_width, bit_width
        );
        return Err(SystemError::ENOSYS);
    }

    return Ok(address);
}

/// # 获取GAS寄存器的最优访问位宽
fn get_access_bit_width(address: u64, register: GenericAddress, max_bit_width: u8) -> u8 {
    let mut access_bit_width;
    let mut max_bit_width = max_bit_width;

    if register.bit_offset == 0
        && register.bit_width != 0
        && is_power_of_two(register.bit_width)
        && is_aligned(register.bit_width as u64, 8)
    {
        // 如果bit_offset为0，且bit_wdith是8/16/32/64，则直接使用bit_width作为访问宽度
        access_bit_width = register.bit_width;
    } else if register.access_size != AccessSize::Undefined {
        // 如果access_size存在，则直接使用access_size作为访问宽度
        access_bit_width = 1 << (register.access_size as u8 + ACPI_ACCESS_BIT_SHIFT);
    } else {
        // 如果access_size不存在，则基于bit_offset和bit_width计算访问宽度
        access_bit_width = round_up_power_of_two_8(register.bit_offset + register.bit_width);
        if access_bit_width <= 8 {
            // 如果计算出的访问宽度小于 8 位，则强制设为 8 位
            access_bit_width = 8;
        } else {
            while !is_aligned(address, access_bit_width >> 3) {
                // 确保地址对齐，若不对齐则降低访问宽度
                access_bit_width >>= 1;
            }
        }
    }

    // IO地址空间的最大访问宽度为32位
    if register.address_space == AddressSpace::SystemIo {
        max_bit_width = 32;
    }

    // 根据请求的最大访问位宽和计算出的访问位宽，选择最小的访问位宽
    if access_bit_width < max_bit_width {
        return access_bit_width;
    }

    return max_bit_width;
}

/// # 检查一个8位无符号整数是否按照s字节对齐
#[inline(always)]
fn is_aligned(a: u64, s: u8) -> bool {
    return (a & (s as u64 - 1)) == 0;
}

#[inline(always)]
fn is_power_of_two(a: u8) -> bool {
    a != 0 && (a & (a - 1)) == 0
}

/// # 将一个8位无符号整数a向上舍入到最接近的2的幂
#[inline(always)]
fn round_up_power_of_two_8(a: u8) -> u8 {
    if a == 0 {
        return 1;
    }
    let highest_bit = 8 - a.wrapping_sub(1).leading_zeros() as u8;
    return 1 << highest_bit;
}

/// # 用于向上对齐value到boundary的整数倍，确保value是boundary的倍数
#[inline(always)]
fn round_up(value: u8, boundary: u8) -> u8 {
    return (value + boundary - 1) & !(boundary - 1);
}

/// # 提取某个变量中的特定位段
#[inline(always)]
fn get_bits(source: u64, position: u8, mask: u64) -> u64 {
    (source >> position) & mask
}

/// # 用于生成从一个最低位到position-1位全为1，而position及更高位全为0的掩码
fn mask_bits_above(position: u8) -> u64 {
    if position == 64 {
        return u64::MAX;
    }
    return !(u64::MAX << position);
}

unsafe fn read_io(port: u16, width: u32) -> Result<u32, SystemError> {
    match width {
        8 => Ok(CurrentPortIOArch::in8(port) as u32),
        16 => Ok(CurrentPortIOArch::in16(port) as u32),
        32 => Ok(CurrentPortIOArch::in32(port)),
        _ => Err(SystemError::EINVAL),
    }
}

unsafe fn write_io(port: u16, value: u32, width: u32) -> Result<(), SystemError> {
    match width {
        8 => CurrentPortIOArch::out8(port, value as u8),
        16 => CurrentPortIOArch::out16(port, value as u16),
        32 => CurrentPortIOArch::out32(port, value),
        _ => return Err(SystemError::EINVAL),
    }

    Ok(())
}

/// # 从内存地址读取值
fn read_memory(paddr: usize, width: u32) -> Result<u64, SystemError> {
    // 读取数据的大小（字节）
    let size = width / 8;
    // 标志是否需要解除映射
    let mut unmap = false;

    let mut vaddr = ACPI_MAP_LIST.find_vaddr(paddr, size as usize);
    if vaddr.is_none() {
        // 如果没有找到对应的虚拟地址，则进行映射
        vaddr = Some(
            EarlyIoRemap::map(PhysAddr::new(paddr), size as usize, false)
                .map(|(vaddr, _)| vaddr.data())?,
        );
        unmap = true;
    }

    let value = match width {
        8 => unsafe { ptr::read_volatile(vaddr.unwrap() as *const u8) as u64 },
        16 => unsafe { ptr::read_volatile(vaddr.unwrap() as *const u16) as u64 },
        32 => unsafe { ptr::read_volatile(vaddr.unwrap() as *const u32) as u64 },
        64 => unsafe { ptr::read_volatile(vaddr.unwrap() as *const u64) },
        _ => {
            error!("acpi read memory error, unsupported width: {}", width);
            return Err(SystemError::EINVAL);
        }
    };

    if unmap {
        // 解除上面的映射
        EarlyIoRemap::unmap(VirtAddr::new(vaddr.unwrap()))?;
    }

    return Ok(value);
}

/// # 向内存地址写入值
fn write_memory(paddr: usize, value: u64, width: u32) -> Result<(), SystemError> {
    // 写入数据的大小（字节）
    let size = width / 8;
    // 标志是否需要解除映射
    let mut unmap = false;

    // 从映射表中查找物理地址对应的虚拟地址(由于目前acpi并没有建立任何物理地址到虚拟地址的映射，所以这里肯定是找不到的)
    let mut vaddr = ACPI_MAP_LIST.find_vaddr(paddr, size as usize);
    if vaddr.is_none() {
        // 如果没有找到对应的虚拟地址，则进行映射
        vaddr = Some(
            EarlyIoRemap::map(PhysAddr::new(paddr), size as usize, false)
                .map(|(vaddr, _)| vaddr.data())?,
        );
        unmap = true;
    }

    match width {
        8 => unsafe {
            ptr::write_volatile(vaddr.unwrap() as *mut u8, value as u8);
        },
        16 => unsafe {
            ptr::write_volatile(vaddr.unwrap() as *mut u16, value as u16);
        },
        32 => unsafe {
            ptr::write_volatile(vaddr.unwrap() as *mut u32, value as u32);
        },
        64 => unsafe { ptr::write_volatile(vaddr.unwrap() as *mut u64, value) },
        _ => {
            error!("acpi write memory error, unsupported width: {}", width);
            return Err(SystemError::EINVAL);
        }
    }

    if unmap {
        // 解除上面的映射
        EarlyIoRemap::unmap(VirtAddr::new(vaddr.unwrap()))?;
    }

    return Ok(());
}
