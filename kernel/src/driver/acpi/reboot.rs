use core::{hint::spin_loop, ptr, slice};

use acpi::{
    address::{AccessSize, AddressSpace, GenericAddress},
    fadt::Fadt,
    AcpiError, AcpiHandler, AcpiTable,
};
use alloc::{boxed::Box, format, string::String, sync::Arc, vec::Vec};
use aml::{value::Args, AmlContext, AmlError, AmlName, AmlValue, DebugVerbosity, Handler};
use log::{debug, error, info, warn};
use system_error::SystemError;

use crate::{
    arch::{io::PortIOArch, CurrentPortIOArch},
    driver::pci::{
        pci::BusDeviceFunction,
        root::{pci_root_0, pci_root_manager},
    },
    libs::spinlock::SpinLock,
    misc::reboot::{register_power_off_handler, PowerOffHandler},
    time::{sleep::nanosleep, PosixTimeSpec},
};

use super::{acpi_manager, AcpiHandlerImpl};

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

#[derive(Debug, Clone, Copy)]
enum AcpiPowerOffMode {
    Legacy {
        pm1a_control: GenericAddress,
        pm1b_control: Option<GenericAddress>,
    },
    Extended {
        sleep_control: GenericAddress,
    },
}

#[derive(Debug, Clone, Copy)]
struct AcpiPowerOffInfo {
    slp_typa: u16,
    slp_typb: u16,
    mode: AcpiPowerOffMode,
}

static ACPI_POWER_OFF_INFO: SpinLock<Option<AcpiPowerOffInfo>> = SpinLock::new(None);
static ACPI_POWER_OFF_PROBE_STATUS: SpinLock<Option<String>> = SpinLock::new(None);

#[derive(Debug)]
struct AcpiPowerOffHandler;

lazy_static! {
    static ref ACPI_POWER_OFF_HANDLER: Arc<AcpiPowerOffHandler> = Arc::new(AcpiPowerOffHandler);
}

impl PowerOffHandler for AcpiPowerOffHandler {
    fn name(&self) -> &'static str {
        "acpi-s5"
    }

    fn priority(&self) -> i32 {
        100
    }

    fn prepare(&self) -> Result<(), SystemError> {
        let tables = acpi_manager().tables().ok_or(SystemError::ENODEV)?;
        let fadt = tables
            .find_table::<Fadt>()
            .map_err(|_| SystemError::ENODEV)?;
        acpi_try_enable(&fadt);
        Ok(())
    }

    fn power_off(&self) -> Result<(), SystemError> {
        acpi_power_off()
    }
}

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
    let info = ACPI_POWER_OFF_INFO
        .lock()
        .as_ref()
        .copied()
        .ok_or_else(|| {
            warn!("ACPI poweroff invoked without registered S5 poweroff info");
            SystemError::ENODEV
        })?;

    let tables = acpi_manager().tables().ok_or_else(|| {
        warn!("ACPI poweroff failed: ACPI tables are unavailable");
        SystemError::ENODEV
    })?;
    let fadt = tables.find_table::<Fadt>().map_err(|_| {
        warn!("ACPI poweroff failed: FADT table is unavailable");
        SystemError::ENODEV
    })?;

    acpi_try_enable(&fadt);

    match info.mode {
        AcpiPowerOffMode::Legacy {
            pm1a_control,
            pm1b_control,
        } => {
            let current_control = acpi_read_pm1_control_pair(pm1a_control, pm1b_control)? as u16;
            let pm1_base =
                (current_control & !ACPI_PM1_CONTROL_WRITEONLY_BITS) & !ACPI_PM1_SLEEP_MASK;
            let pm1a_value = pm1_base | ((info.slp_typa & 0x7) << ACPI_PM1_SLEEP_TYPE_SHIFT);
            let pm1b_value = pm1_base | ((info.slp_typb & 0x7) << ACPI_PM1_SLEEP_TYPE_SHIFT);

            acpi_write_pm1_control_pair(pm1a_control, pm1a_value, pm1b_control, pm1b_value)?;
            acpi_write_pm1_control_pair(
                pm1a_control,
                pm1a_value | ACPI_PM1_SLEEP_ENABLE,
                pm1b_control,
                pm1b_value | ACPI_PM1_SLEEP_ENABLE,
            )?;
        }
        AcpiPowerOffMode::Extended { sleep_control } => {
            let value = (((info.slp_typa & 0x7) << ACPI_SLEEP_CONTROL_TYPE_SHIFT)
                | ACPI_SLEEP_CONTROL_ENABLE) as u64;
            acpi_write_gas(sleep_control, value)?;
        }
    }

    for _ in 0..ACPI_POWER_OFF_SPIN_LOOPS {
        spin_loop();
    }

    Err(SystemError::EIO)
}

pub fn register_acpi_poweroff_handler() {
    let info = match probe_acpi_power_off() {
        Ok(info) => info,
        Err(e) => {
            warn!("ACPI poweroff handler not registered: {:?}", e);
            return;
        }
    };

    *ACPI_POWER_OFF_INFO.lock() = Some(info);
    set_probe_status(match info.mode {
        AcpiPowerOffMode::Legacy { .. } => "registered ACPI S5 legacy PM1 poweroff handler".into(),
        AcpiPowerOffMode::Extended { .. } => {
            "registered ACPI S5 extended sleep control poweroff handler".into()
        }
    });

    if let Err(e) = register_power_off_handler(ACPI_POWER_OFF_HANDLER.clone()) {
        warn!("ACPI poweroff handler registration failed: {:?}", e);
        set_probe_status("ACPI S5 probe succeeded but handler registration failed".into());
        return;
    }

    info!("ACPI S5 poweroff handler registered");
}

fn probe_acpi_power_off() -> Result<AcpiPowerOffInfo, SystemError> {
    let tables = acpi_manager().tables().ok_or_else(|| {
        set_probe_status("ACPI tables are unavailable".into());
        warn!("ACPI poweroff probe failed: ACPI tables are unavailable");
        SystemError::ENODEV
    })?;
    let fadt = tables.find_table::<Fadt>().map_err(|_| {
        set_probe_status("FADT table is unavailable".into());
        warn!("ACPI poweroff probe failed: FADT table is unavailable");
        SystemError::ENODEV
    })?;
    let (slp_typa, slp_typb) = find_s5_sleep_type().map_err(|e| {
        let status = describe_s5_lookup_error(&e);
        set_probe_status(status.clone());
        warn!("ACPI poweroff probe failed: {}", status);
        SystemError::ENODEV
    })?;

    let pm1a_control = fadt.pm1a_control_block().map_err(|_| {
        set_probe_status("PM1A control block is invalid".into());
        warn!("ACPI poweroff probe failed: PM1A control block is invalid");
        SystemError::ENODEV
    })?;
    if pm1a_control.address != 0 {
        let pm1b_control = fadt
            .pm1b_control_block()
            .map_err(|_| {
                set_probe_status("PM1B control block is invalid".into());
                warn!("ACPI poweroff probe failed: PM1B control block is invalid");
                SystemError::ENODEV
            })?
            .filter(|register| register.address != 0);

        return Ok(AcpiPowerOffInfo {
            slp_typa,
            slp_typb,
            mode: AcpiPowerOffMode::Legacy {
                pm1a_control,
                pm1b_control,
            },
        });
    }

    if let Some(sleep_control) = fadt
        .sleep_control_register()
        .map_err(|_| {
            set_probe_status("sleep control register is invalid".into());
            warn!("ACPI poweroff probe failed: sleep control register is invalid");
            SystemError::ENODEV
        })?
        .filter(|register| register.address != 0)
    {
        return Ok(AcpiPowerOffInfo {
            slp_typa,
            slp_typb,
            mode: AcpiPowerOffMode::Extended { sleep_control },
        });
    }

    set_probe_status("neither PM1 control nor sleep control register is available".into());
    warn!(
        "ACPI poweroff probe failed: neither PM1 control nor sleep control register is available"
    );
    return Err(SystemError::ENODEV);
}

pub fn acpi_poweroff_probe_status() -> String {
    ACPI_POWER_OFF_PROBE_STATUS
        .lock()
        .clone()
        .unwrap_or_else(|| "ACPI poweroff probe has not run".into())
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

#[derive(Debug)]
enum S5LookupError {
    DsdtLookupFailed {
        error: AcpiError,
        fadt_snapshot: String,
    },
    DsdtMapFailed {
        address: usize,
        length: u32,
        error: SystemError,
    },
    DsdtParseFailed {
        raw_s5_in_dsdt: bool,
        error: AmlError,
    },
    SsdtWithS5ParseFailed {
        index: usize,
        raw_s5_in_dsdt: bool,
        error: AmlError,
    },
    NamespaceLookupFailed {
        raw_s5_in_dsdt: bool,
        raw_s5_in_ssdt: bool,
        error: AmlError,
    },
    InvalidSleepTypePackage {
        raw_s5_in_dsdt: bool,
        raw_s5_in_ssdt: bool,
    },
    RawAmlDoesNotContainS5 {
        raw_s5_in_dsdt: bool,
        raw_s5_in_ssdt: bool,
    },
}

fn find_s5_sleep_type() -> Result<(u16, u16), S5LookupError> {
    let tables = acpi_manager()
        .tables()
        .ok_or(S5LookupError::DsdtLookupFailed {
            error: AcpiError::InvalidDsdtAddress,
            fadt_snapshot: "ACPI tables are unavailable while probing DSDT".into(),
        })?;
    let mut context = AmlContext::new(Box::new(AcpiAmlHandler), DebugVerbosity::None);

    let fadt = tables
        .find_table::<Fadt>()
        .map_err(|_| S5LookupError::DsdtLookupFailed {
            error: AcpiError::TableMissing(Fadt::SIGNATURE),
            fadt_snapshot: "FADT lookup failed while probing DSDT".into(),
        })?;
    let dsdt = tables
        .dsdt()
        .map_err(|error| S5LookupError::DsdtLookupFailed {
            error,
            fadt_snapshot: format!("{:?}", &*fadt),
        })?;
    let dsdt_bytes = map_aml_table_bytes(&dsdt).map_err(|error| S5LookupError::DsdtMapFailed {
        address: dsdt.address,
        length: dsdt.length,
        error,
    })?;
    let raw_s5_in_dsdt = aml_stream_contains_s5(&dsdt_bytes);
    parse_aml_stream(&mut context, &dsdt_bytes, "DSDT").map_err(|error| {
        S5LookupError::DsdtParseFailed {
            raw_s5_in_dsdt,
            error,
        }
    })?;

    let mut raw_s5_in_ssdt = false;
    let mut ssdt_with_s5_parse_error = None;
    for (index, ssdt) in tables.ssdts().enumerate() {
        let ssdt_bytes = match map_aml_table_bytes(&ssdt) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        let ssdt_has_raw_s5 = aml_stream_contains_s5(&ssdt_bytes);
        raw_s5_in_ssdt |= ssdt_has_raw_s5;
        if let Err(error) = parse_aml_stream(&mut context, &ssdt_bytes, "SSDT") {
            if ssdt_has_raw_s5 && ssdt_with_s5_parse_error.is_none() {
                ssdt_with_s5_parse_error = Some((index, error));
            }
        }
    }

    match find_s5_sleep_type_in_context(&mut context, raw_s5_in_dsdt, raw_s5_in_ssdt) {
        Ok(value) => Ok(value),
        Err(S5LookupError::RawAmlDoesNotContainS5 { .. }) => {
            if let Some((index, error)) = ssdt_with_s5_parse_error {
                Err(S5LookupError::SsdtWithS5ParseFailed {
                    index,
                    raw_s5_in_dsdt,
                    error,
                })
            } else {
                Err(S5LookupError::RawAmlDoesNotContainS5 {
                    raw_s5_in_dsdt,
                    raw_s5_in_ssdt,
                })
            }
        }
        Err(error) => Err(error),
    }
}

fn map_aml_table_bytes(table: &acpi::AmlTable) -> Result<Vec<u8>, SystemError> {
    let mapping =
        unsafe { AcpiHandlerImpl.map_physical_region::<u8>(table.address, table.length as usize) };
    let bytes =
        unsafe { slice::from_raw_parts(mapping.virtual_start().as_ptr(), table.length as usize) };
    let owned = bytes.to_vec();
    Ok(owned)
}

fn aml_stream_contains_s5(bytes: &[u8]) -> bool {
    bytes.windows(4).any(|window| window == b"_S5_")
}

fn parse_aml_stream(context: &mut AmlContext, bytes: &[u8], name: &str) -> Result<(), AmlError> {
    context.parse_table(bytes).map_err(|e| {
        warn!("failed to parse ACPI {} for _S5 lookup: {:?}", name, e);
        e
    })
}

fn find_s5_sleep_type_in_context(
    context: &mut AmlContext,
    raw_s5_in_dsdt: bool,
    raw_s5_in_ssdt: bool,
) -> Result<(u16, u16), S5LookupError> {
    let s5_name = AmlName::from_str("\\_S5_").unwrap();
    let value = match context.namespace.get_by_path(&s5_name) {
        Ok(value) => match value.clone() {
            AmlValue::Method { .. } => {
                context
                    .invoke_method(&s5_name, Args::EMPTY)
                    .map_err(|error| S5LookupError::NamespaceLookupFailed {
                        raw_s5_in_dsdt,
                        raw_s5_in_ssdt,
                        error,
                    })?
            }
            value => value,
        },
        Err(error) => {
            if raw_s5_in_dsdt || raw_s5_in_ssdt {
                return Err(S5LookupError::NamespaceLookupFailed {
                    raw_s5_in_dsdt,
                    raw_s5_in_ssdt,
                    error,
                });
            }

            return Err(S5LookupError::RawAmlDoesNotContainS5 {
                raw_s5_in_dsdt,
                raw_s5_in_ssdt,
            });
        }
    };

    s5_sleep_type_from_value(&value).ok_or(S5LookupError::InvalidSleepTypePackage {
        raw_s5_in_dsdt,
        raw_s5_in_ssdt,
    })
}

fn describe_s5_lookup_error(error: &S5LookupError) -> String {
    match error {
        S5LookupError::DsdtLookupFailed {
            error,
            fadt_snapshot,
        } => format!(
            "DSDT lookup failed: {:?}; FADT snapshot: {}",
            error, fadt_snapshot
        ),
        S5LookupError::DsdtMapFailed {
            address,
            length,
            error,
        } => format!(
            "DSDT AML mapping failed: {:?} (address={:#x}, length={:#x})",
            error, address, length
        ),
        S5LookupError::DsdtParseFailed {
            raw_s5_in_dsdt,
            error,
        } => format!(
            "DSDT AML parse failed before _S5 lookup: {:?} (raw AML contains _S5_: {})",
            error,
            yes_no(*raw_s5_in_dsdt)
        ),
        S5LookupError::SsdtWithS5ParseFailed {
            index,
            raw_s5_in_dsdt,
            error,
        } => format!(
            "SSDT[{}] AML parse failed while raw AML contains _S5_: {:?} (DSDT raw _S5_: {})",
            index,
            error,
            yes_no(*raw_s5_in_dsdt)
        ),
        S5LookupError::NamespaceLookupFailed {
            raw_s5_in_dsdt,
            raw_s5_in_ssdt,
            error,
        } => format!(
            "_S5_ bytes exist in raw AML but namespace lookup/evaluation failed: {:?} (DSDT raw _S5_: {}, SSDT raw _S5_: {})",
            error,
            yes_no(*raw_s5_in_dsdt),
            yes_no(*raw_s5_in_ssdt)
        ),
        S5LookupError::InvalidSleepTypePackage {
            raw_s5_in_dsdt,
            raw_s5_in_ssdt,
        } => format!(
            "_S5_ exists but does not evaluate to a valid sleep-type package (DSDT raw _S5_: {}, SSDT raw _S5_: {})",
            yes_no(*raw_s5_in_dsdt),
            yes_no(*raw_s5_in_ssdt)
        ),
        S5LookupError::RawAmlDoesNotContainS5 {
            raw_s5_in_dsdt,
            raw_s5_in_ssdt,
        } => format!(
            "raw AML does not contain _S5_ (DSDT raw _S5_: {}, SSDT raw _S5_: {})",
            yes_no(*raw_s5_in_dsdt),
            yes_no(*raw_s5_in_ssdt)
        ),
    }
}

fn set_probe_status(status: String) {
    *ACPI_POWER_OFF_PROBE_STATUS.lock() = Some(status);
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

fn s5_sleep_type_from_value(value: &AmlValue) -> Option<(u16, u16)> {
    match value {
        AmlValue::Package(elements) => s5_sleep_type_from_package(elements),
        _ => None,
    }
}

fn s5_sleep_type_from_package(elements: &[AmlValue]) -> Option<(u16, u16)> {
    match elements.len() {
        0 => None,
        1 => {
            let encoded = aml_integer(&elements[0])?;
            Some(((encoded & 0xff) as u16, ((encoded >> 8) & 0xff) as u16))
        }
        _ => Some((
            (aml_integer(&elements[0])? & 0xff) as u16,
            (aml_integer(&elements[1])? & 0xff) as u16,
        )),
    }
}

fn aml_integer(value: &AmlValue) -> Option<u64> {
    match value {
        AmlValue::Integer(value) => Some(*value),
        _ => None,
    }
}

struct AcpiAmlHandler;

impl Handler for AcpiAmlHandler {
    fn read_u8(&self, address: usize) -> u8 {
        read_memory(address, 8).unwrap_or_else(|e| {
            warn!("AML memory read8 failed at {:#x}: {:?}", address, e);
            0
        }) as u8
    }

    fn read_u16(&self, address: usize) -> u16 {
        read_memory(address, 16).unwrap_or_else(|e| {
            warn!("AML memory read16 failed at {:#x}: {:?}", address, e);
            0
        }) as u16
    }

    fn read_u32(&self, address: usize) -> u32 {
        read_memory(address, 32).unwrap_or_else(|e| {
            warn!("AML memory read32 failed at {:#x}: {:?}", address, e);
            0
        }) as u32
    }

    fn read_u64(&self, address: usize) -> u64 {
        read_memory(address, 64).unwrap_or_else(|e| {
            warn!("AML memory read64 failed at {:#x}: {:?}", address, e);
            0
        })
    }

    fn write_u8(&mut self, address: usize, value: u8) {
        if let Err(e) = write_memory(address, value as u64, 8) {
            warn!("AML memory write8 failed at {:#x}: {:?}", address, e);
        }
    }

    fn write_u16(&mut self, address: usize, value: u16) {
        if let Err(e) = write_memory(address, value as u64, 16) {
            warn!("AML memory write16 failed at {:#x}: {:?}", address, e);
        }
    }

    fn write_u32(&mut self, address: usize, value: u32) {
        if let Err(e) = write_memory(address, value as u64, 32) {
            warn!("AML memory write32 failed at {:#x}: {:?}", address, e);
        }
    }

    fn write_u64(&mut self, address: usize, value: u64) {
        if let Err(e) = write_memory(address, value, 64) {
            warn!("AML memory write64 failed at {:#x}: {:?}", address, e);
        }
    }

    fn read_io_u8(&self, port: u16) -> u8 {
        unsafe { read_io(port, 8).unwrap_or(0) as u8 }
    }

    fn read_io_u16(&self, port: u16) -> u16 {
        unsafe { read_io(port, 16).unwrap_or(0) as u16 }
    }

    fn read_io_u32(&self, port: u16) -> u32 {
        unsafe { read_io(port, 32).unwrap_or(0) }
    }

    fn write_io_u8(&self, port: u16, value: u8) {
        let _ = unsafe { write_io(port, value as u32, 8) };
    }

    fn write_io_u16(&self, port: u16, value: u16) {
        let _ = unsafe { write_io(port, value as u32, 16) };
    }

    fn write_io_u32(&self, port: u16, value: u32) {
        let _ = unsafe { write_io(port, value, 32) };
    }

    fn read_pci_u8(&self, segment: u16, bus: u8, device: u8, function: u8, offset: u16) -> u8 {
        read_pci_config_u8(segment, bus, device, function, offset)
    }

    fn read_pci_u16(&self, segment: u16, bus: u8, device: u8, function: u8, offset: u16) -> u16 {
        (read_pci_config_u8(segment, bus, device, function, offset) as u16)
            | ((read_pci_config_u8(segment, bus, device, function, offset + 1) as u16) << 8)
    }

    fn read_pci_u32(&self, segment: u16, bus: u8, device: u8, function: u8, offset: u16) -> u32 {
        let Some(root) = pci_root_manager().get_pci_root(segment) else {
            return 0;
        };
        if offset & 0x3 != 0 {
            return (read_pci_config_u8(segment, bus, device, function, offset) as u32)
                | ((read_pci_config_u8(segment, bus, device, function, offset + 1) as u32) << 8)
                | ((read_pci_config_u8(segment, bus, device, function, offset + 2) as u32) << 16)
                | ((read_pci_config_u8(segment, bus, device, function, offset + 3) as u32) << 24);
        }
        let devfn = BusDeviceFunction {
            bus,
            device,
            function,
        };
        root.read_config(devfn, offset & !0x3)
    }

    fn write_pci_u8(
        &self,
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
        offset: u16,
        value: u8,
    ) {
        write_pci_config_u8(segment, bus, device, function, offset, value);
    }

    fn write_pci_u16(
        &self,
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
        offset: u16,
        value: u16,
    ) {
        write_pci_config_u8(segment, bus, device, function, offset, value as u8);
        write_pci_config_u8(
            segment,
            bus,
            device,
            function,
            offset + 1,
            (value >> 8) as u8,
        );
    }

    fn write_pci_u32(
        &self,
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
        offset: u16,
        value: u32,
    ) {
        let Some(root) = pci_root_manager().get_pci_root(segment) else {
            return;
        };
        if offset & 0x3 != 0 {
            write_pci_config_u8(segment, bus, device, function, offset, value as u8);
            write_pci_config_u8(
                segment,
                bus,
                device,
                function,
                offset + 1,
                (value >> 8) as u8,
            );
            write_pci_config_u8(
                segment,
                bus,
                device,
                function,
                offset + 2,
                (value >> 16) as u8,
            );
            write_pci_config_u8(
                segment,
                bus,
                device,
                function,
                offset + 3,
                (value >> 24) as u8,
            );
            return;
        }
        let devfn = BusDeviceFunction {
            bus,
            device,
            function,
        };
        root.write_config(devfn, offset & !0x3, value);
    }

    fn stall(&self, microseconds: u64) {
        let time = PosixTimeSpec {
            tv_sec: (microseconds / 1_000_000) as i64,
            tv_nsec: ((microseconds % 1_000_000) * 1_000) as i64,
        };
        let _ = nanosleep(time);
    }

    fn sleep(&self, milliseconds: u64) {
        let time = PosixTimeSpec {
            tv_sec: (milliseconds / 1_000) as i64,
            tv_nsec: ((milliseconds % 1_000) * 1_000_000) as i64,
        };
        let _ = nanosleep(time);
    }
}

fn read_pci_config_u8(segment: u16, bus: u8, device: u8, function: u8, offset: u16) -> u8 {
    let Some(root) = pci_root_manager().get_pci_root(segment) else {
        return 0;
    };
    let devfn = BusDeviceFunction {
        bus,
        device,
        function,
    };
    let shift = ((offset & 0x3) * 8) as u32;
    ((root.read_config(devfn, offset & !0x3) >> shift) & 0xff) as u8
}

fn write_pci_config_u8(segment: u16, bus: u8, device: u8, function: u8, offset: u16, value: u8) {
    let Some(root) = pci_root_manager().get_pci_root(segment) else {
        return;
    };
    let devfn = BusDeviceFunction {
        bus,
        device,
        function,
    };
    let aligned_offset = offset & !0x3;
    let shift = ((offset & 0x3) * 8) as u32;
    let mask = 0xff << shift;
    let current = root.read_config(devfn, aligned_offset);
    root.write_config(
        devfn,
        aligned_offset,
        (current & !mask) | ((value as u32) << shift),
    );
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

    if let Some(vaddr) = ACPI_MAP_LIST.find_vaddr(paddr, size as usize) {
        return read_bytes_volatile(vaddr as *const u8, width);
    }

    let mapping = unsafe { AcpiHandlerImpl.map_physical_region::<u8>(paddr, size as usize) };
    read_bytes_volatile(mapping.virtual_start().as_ptr(), width)
}

/// # 向内存地址写入值
fn write_memory(paddr: usize, value: u64, width: u32) -> Result<(), SystemError> {
    // 写入数据的大小（字节）
    let size = width / 8;

    // 从映射表中查找物理地址对应的虚拟地址(由于目前acpi并没有建立任何物理地址到虚拟地址的映射，所以这里肯定是找不到的)
    if let Some(vaddr) = ACPI_MAP_LIST.find_vaddr(paddr, size as usize) {
        return write_bytes_volatile(vaddr as *mut u8, value, width);
    }

    let mapping = unsafe { AcpiHandlerImpl.map_physical_region::<u8>(paddr, size as usize) };
    write_bytes_volatile(mapping.virtual_start().as_ptr(), value, width)
}

fn read_bytes_volatile(ptr: *const u8, width: u32) -> Result<u64, SystemError> {
    let byte_count = width_to_byte_count(width)?;
    let mut value = 0u64;
    for index in 0..byte_count {
        let byte = unsafe { ptr::read_volatile(ptr.add(index)) } as u64;
        value |= byte << (index * 8);
    }
    Ok(value)
}

fn write_bytes_volatile(ptr: *mut u8, value: u64, width: u32) -> Result<(), SystemError> {
    let byte_count = width_to_byte_count(width)?;
    for index in 0..byte_count {
        let byte = ((value >> (index * 8)) & 0xff) as u8;
        unsafe {
            ptr::write_volatile(ptr.add(index), byte);
        }
    }
    Ok(())
}

fn width_to_byte_count(width: u32) -> Result<usize, SystemError> {
    match width {
        8 => Ok(1),
        16 => Ok(2),
        32 => Ok(4),
        64 => Ok(8),
        _ => {
            error!("acpi memory access error, unsupported width: {}", width);
            Err(SystemError::EINVAL)
        }
    }
}
