use crate::{
    driver::{
        acpi::acpi_manager,
        base::{kobject::KObject, kset::KSet},
    },
    filesystem::{
        sysfs::{
            file::sysfs_emit_str, sysfs_instance, Attribute, BinAttribute, SysFSOpsSupport,
            SYSFS_ATTR_MODE_RO,
        },
        vfs::syscall::ModeType,
    },
    libs::rwlock::RwLock,
};
use acpi::sdt::SdtHeader;
use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use log::{debug, error, warn};
use system_error::SystemError;

use super::{acpi_kset, AcpiManager};

// 定义所有ACPI表结构体
macro_rules! define_acpi_tables {
    ($($name:ident),*) => {
        $(
            #[repr(transparent)]
            #[allow(non_snake_case)]
            #[allow(non_camel_case_types)]
            struct $name {
                header: SdtHeader,
            }

            unsafe impl acpi::AcpiTable for $name {
                const SIGNATURE: acpi::sdt::Signature = acpi::sdt::Signature::$name;
                fn header(&self) -> &acpi::sdt::SdtHeader {
                    return &self.header;
                }
            }
        )*
    };
}

define_acpi_tables!(
    RSDT, XSDT, FADT, HPET, MADT, MCFG, SSDT, BERT, BGRT, CPEP, DSDT, ECDT, EINJ, ERST, FACS, FPDT,
    GTDT, HEST, MSCT, MPST, NFIT, PCCT, PHAT, PMTT, PSDT, RASF, SBST, SDEV, SLIT, SRAT, AEST, BDAT,
    CDIT, CEDT, CRAT, CSRT, DBGP, DBG2, DMAR, DRTM, ETDT, IBFT, IORT, IVRS, LPIT, MCHI, MPAM, MSDM,
    PRMT, RGRT, SDEI, SLIC, SPCR, SPMI, STAO, SVKL, TCPA, TPM2, UEFI, WAET, WDAT, WDRT, WPBT, WSMT,
    XENV
);

macro_rules! handle_read_table {
    ($name: ident, $name_str: expr, $tables: expr, $buf: expr, $offset: expr) => {{
        AttrAcpiTable::do_binattr_read_table::<$name>($tables, $name_str, $buf, $offset)
    }};
}

static mut __HOTPLUG_KSET_INSTANCE: Option<Arc<KSet>> = None;
static mut __ACPI_TABLES_KSET_INSTANCE: Option<Arc<KSet>> = None;
static mut __ACPI_TABLES_DATA_KSET_INSTANCE: Option<Arc<KSet>> = None;
static mut __ACPI_TABLES_DYNAMIC_KSET_INSTANCE: Option<Arc<KSet>> = None;
static mut __ACPI_TABLE_ATTR_LIST: Option<RwLock<Vec<Arc<AttrAcpiTable>>>> = None;

const ACPI_MAX_TABLE_INSTANCES: usize = 999;

#[inline(always)]
#[allow(dead_code)]
pub fn hotplug_kset() -> Arc<KSet> {
    unsafe { __HOTPLUG_KSET_INSTANCE.clone().unwrap() }
}

#[inline(always)]
pub fn acpi_tables_kset() -> Arc<KSet> {
    unsafe { __ACPI_TABLES_KSET_INSTANCE.clone().unwrap() }
}

#[inline(always)]
#[allow(dead_code)]
pub fn acpi_tables_data_kset() -> Arc<KSet> {
    unsafe { __ACPI_TABLES_DATA_KSET_INSTANCE.clone().unwrap() }
}

#[inline(always)]
#[allow(dead_code)]
pub fn acpi_tables_dynamic_kset() -> Arc<KSet> {
    unsafe { __ACPI_TABLES_DYNAMIC_KSET_INSTANCE.clone().unwrap() }
}

#[inline(always)]
fn acpi_table_attr_list() -> &'static RwLock<Vec<Arc<AttrAcpiTable>>> {
    unsafe {
        return __ACPI_TABLE_ATTR_LIST.as_ref().unwrap();
    }
}

impl AcpiManager {
    pub(super) fn acpi_sysfs_init(&self) -> Result<(), SystemError> {
        unsafe {
            __ACPI_TABLE_ATTR_LIST = Some(RwLock::new(Vec::new()));
        }
        self.acpi_tables_sysfs_init()?;

        let hotplug_kset = KSet::new("hotplug".to_string());
        hotplug_kset.register(Some(acpi_kset()))?;

        unsafe {
            __HOTPLUG_KSET_INSTANCE = Some(hotplug_kset.clone());
        }

        let hotplug_kobj = hotplug_kset as Arc<dyn KObject>;
        sysfs_instance().create_file(&hotplug_kobj, &AttrForceRemove)?;

        return Ok(());
    }

    /// 在 sysfs 中创建 ACPI 表目录
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/acpi/sysfs.c?fi=acpi_sysfs_init#488
    fn acpi_tables_sysfs_init(&self) -> Result<(), SystemError> {
        // 创建 `/sys/firmware/acpi/tables` 目录
        let acpi_tables_kset = KSet::new("tables".to_string());
        acpi_tables_kset.register(Some(acpi_kset()))?;
        unsafe {
            __ACPI_TABLES_KSET_INSTANCE = Some(acpi_tables_kset.clone());
        }

        // 创建 `/sys/firmware/acpi/tables/data` 目录
        let acpi_tables_data_kset = KSet::new("data".to_string());
        acpi_tables_data_kset.register(Some(acpi_tables_kset.clone()))?;
        unsafe {
            __ACPI_TABLES_DATA_KSET_INSTANCE = Some(acpi_tables_data_kset);
        }

        // 创建 `/sys/firmware/acpi/tables/dynamic` 目录
        let acpi_tables_dynamic_kset = KSet::new("dynamic".to_string());
        acpi_tables_dynamic_kset.register(Some(acpi_tables_kset.clone()))?;
        unsafe {
            __ACPI_TABLES_DYNAMIC_KSET_INSTANCE = Some(acpi_tables_dynamic_kset);
        }

        // todo: get acpi tables.
        let tables = self.tables().unwrap();
        let headers = tables.headers();
        for header in headers {
            debug!("ACPI header: {:?}", header);
            let attr = AttrAcpiTable::new(&header)?;
            acpi_table_attr_list().write().push(attr);
            self.acpi_table_data_init(&header)?;
        }

        return Ok(());
    }

    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/acpi/sysfs.c?fi=acpi_sysfs_init#469
    fn acpi_table_data_init(&self, _header: &SdtHeader) -> Result<(), SystemError> {
        // todo!("AcpiManager::acpi_table_data_init()")
        return Ok(());
    }
}

#[derive(Debug)]
struct AttrForceRemove;

impl Attribute for AttrForceRemove {
    fn name(&self) -> &str {
        "force_remove"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        return SysFSOpsSupport::ATTR_SHOW;
    }

    fn show(&self, _kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        return sysfs_emit_str(buf, "0\n");
    }
}

/// ACPI 表在 sysfs 中的属性
#[derive(Debug)]
struct AttrAcpiTable {
    name: String,
    filename: String,
    instance: isize,
    size: usize,
}

impl AttrAcpiTable {
    pub fn new(header: &SdtHeader) -> Result<Arc<Self>, SystemError> {
        let mut r = Self {
            name: header.signature.to_string(),
            filename: "".to_string(),
            instance: 0,
            size: header.length as usize,
        };

        for attr in acpi_table_attr_list().read().iter() {
            if attr.name == r.name {
                r.instance = attr.instance;
            }
        }
        // 将当前实例的序号加1
        r.instance += 1;
        if r.instance > ACPI_MAX_TABLE_INSTANCES as isize {
            warn!("too many table instances. name: {}", r.name);
            return Err(SystemError::ERANGE);
        }

        let mut has_multiple_instances: bool = false;
        let mut tmpcnt = 0;
        for h in acpi_manager().tables().unwrap().headers() {
            if h.signature == header.signature {
                tmpcnt += 1;
                if tmpcnt > 1 {
                    has_multiple_instances = true;
                    break;
                }
            }
        }

        if r.instance > 1 || (r.instance == 1 && has_multiple_instances) {
            r.filename = format!("{}{}", r.name, r.instance);
        } else {
            r.filename = r.name.clone();
        }

        let result = Arc::new(r);
        sysfs_instance().create_bin_file(
            &(acpi_tables_kset() as Arc<dyn KObject>),
            &(result.clone() as Arc<dyn BinAttribute>),
        )?;
        return Ok(result);
    }

    #[inline(never)]
    fn do_binattr_read_table<T: acpi::AcpiTable>(
        tables: &'static acpi::AcpiTables<crate::driver::acpi::AcpiHandlerImpl>,
        name: &str,
        buf: &mut [u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        let table = tables.find_entire_table::<T>().map_err(|e| {
            warn!(
                "AttrAcpiTable::read(): failed to find table. name: {}, error: {:?}",
                name, e
            );
            SystemError::ENODEV
        })?;

        let from = unsafe {
            core::slice::from_raw_parts(
                table.virtual_start().as_ptr() as *const u8,
                table.region_length(),
            )
        };
        if offset >= from.len() {
            return Ok(0);
        }
        let mut count = buf.len();
        if count > from.len() - offset {
            count = from.len() - offset;
        }
        buf[0..count].copy_from_slice(&from[offset..offset + count]);
        return Ok(count);
    }

    #[inline(never)]
    fn do_binattr_read_1(
        &self,
        tables: &'static acpi::AcpiTables<crate::driver::acpi::AcpiHandlerImpl>,
        name_str: &str,
        buf: &mut [u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        match name_str {
            "RSDT" => {
                handle_read_table!(RSDT, name_str, tables, buf, offset)
            }
            "XSDT" => {
                handle_read_table!(XSDT, name_str, tables, buf, offset)
            }
            "FACP" => {
                handle_read_table!(FADT, name_str, tables, buf, offset)
            }
            "HPET" => {
                handle_read_table!(HPET, name_str, tables, buf, offset)
            }
            "APIC" => {
                handle_read_table!(MADT, name_str, tables, buf, offset)
            }
            "MCFG" => {
                handle_read_table!(MCFG, name_str, tables, buf, offset)
            }
            "SSDT" => {
                handle_read_table!(SSDT, name_str, tables, buf, offset)
            }
            "BERT" => {
                handle_read_table!(BERT, name_str, tables, buf, offset)
            }
            "BGRT" => {
                handle_read_table!(BGRT, name_str, tables, buf, offset)
            }
            "CPEP" => {
                handle_read_table!(CPEP, name_str, tables, buf, offset)
            }
            "DSDT" => {
                handle_read_table!(DSDT, name_str, tables, buf, offset)
            }
            "ECDT" => {
                handle_read_table!(ECDT, name_str, tables, buf, offset)
            }
            "EINJ" => {
                handle_read_table!(EINJ, name_str, tables, buf, offset)
            }
            "ERST" => {
                handle_read_table!(ERST, name_str, tables, buf, offset)
            }
            "FACS" => {
                handle_read_table!(FACS, name_str, tables, buf, offset)
            }
            "FPDT" => {
                handle_read_table!(FPDT, name_str, tables, buf, offset)
            }
            "GTDT" => {
                handle_read_table!(GTDT, name_str, tables, buf, offset)
            }
            "HEST" => {
                handle_read_table!(HEST, name_str, tables, buf, offset)
            }

            _ => Err(SystemError::ENODEV),
        }
    }

    #[inline(never)]
    fn do_binattr_read_2(
        &self,
        tables: &'static acpi::AcpiTables<crate::driver::acpi::AcpiHandlerImpl>,
        name_str: &str,
        buf: &mut [u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        match name_str {
            "MSCT" => {
                handle_read_table!(MSCT, name_str, tables, buf, offset)
            }
            "MPST" => {
                handle_read_table!(MPST, name_str, tables, buf, offset)
            }
            "NFIT" => {
                handle_read_table!(NFIT, name_str, tables, buf, offset)
            }
            "PCCT" => {
                handle_read_table!(PCCT, name_str, tables, buf, offset)
            }
            "PHAT" => {
                handle_read_table!(PHAT, name_str, tables, buf, offset)
            }
            "PMTT" => {
                handle_read_table!(PMTT, name_str, tables, buf, offset)
            }
            "PSDT" => {
                handle_read_table!(PSDT, name_str, tables, buf, offset)
            }
            "RASF" => {
                handle_read_table!(RASF, name_str, tables, buf, offset)
            }
            "SBST" => {
                handle_read_table!(SBST, name_str, tables, buf, offset)
            }
            "SDEV" => {
                handle_read_table!(SDEV, name_str, tables, buf, offset)
            }
            "SLIT" => {
                handle_read_table!(SLIT, name_str, tables, buf, offset)
            }
            "SRAT" => {
                handle_read_table!(SRAT, name_str, tables, buf, offset)
            }
            "AEST" => {
                handle_read_table!(AEST, name_str, tables, buf, offset)
            }
            "BDAT" => {
                handle_read_table!(BDAT, name_str, tables, buf, offset)
            }
            "CDIT" => {
                handle_read_table!(CDIT, name_str, tables, buf, offset)
            }
            "CEDT" => {
                handle_read_table!(CEDT, name_str, tables, buf, offset)
            }
            "CRAT" => {
                handle_read_table!(CRAT, name_str, tables, buf, offset)
            }
            "CSRT" => {
                handle_read_table!(CSRT, name_str, tables, buf, offset)
            }
            "DBGP" => {
                handle_read_table!(DBGP, name_str, tables, buf, offset)
            }
            "DBG2" => {
                handle_read_table!(DBG2, name_str, tables, buf, offset)
            }
            "DMAR" => {
                handle_read_table!(DMAR, name_str, tables, buf, offset)
            }
            "DRTM" => {
                handle_read_table!(DRTM, name_str, tables, buf, offset)
            }
            "ETDT" => {
                handle_read_table!(ETDT, name_str, tables, buf, offset)
            }
            "IBFT" => {
                handle_read_table!(IBFT, name_str, tables, buf, offset)
            }
            "IORT" => {
                handle_read_table!(IORT, name_str, tables, buf, offset)
            }
            "IVRS" => {
                handle_read_table!(IVRS, name_str, tables, buf, offset)
            }
            "LPIT" => {
                handle_read_table!(LPIT, name_str, tables, buf, offset)
            }
            "MCHI" => {
                handle_read_table!(MCHI, name_str, tables, buf, offset)
            }
            "MPAM" => {
                handle_read_table!(MPAM, name_str, tables, buf, offset)
            }
            "MSDM" => {
                handle_read_table!(MSDM, name_str, tables, buf, offset)
            }
            "PRMT" => {
                handle_read_table!(PRMT, name_str, tables, buf, offset)
            }
            "RGRT" => {
                handle_read_table!(RGRT, name_str, tables, buf, offset)
            }
            "SDEI" => {
                handle_read_table!(SDEI, name_str, tables, buf, offset)
            }
            "SLIC" => {
                handle_read_table!(SLIC, name_str, tables, buf, offset)
            }
            "SPCR" => {
                handle_read_table!(SPCR, name_str, tables, buf, offset)
            }
            "SPMI" => {
                handle_read_table!(SPMI, name_str, tables, buf, offset)
            }
            "STAO" => {
                handle_read_table!(STAO, name_str, tables, buf, offset)
            }
            "SVKL" => {
                handle_read_table!(SVKL, name_str, tables, buf, offset)
            }
            "TCPA" => {
                handle_read_table!(TCPA, name_str, tables, buf, offset)
            }
            "TPM2" => {
                handle_read_table!(TPM2, name_str, tables, buf, offset)
            }
            "UEFI" => {
                handle_read_table!(UEFI, name_str, tables, buf, offset)
            }
            "WAET" => {
                handle_read_table!(WAET, name_str, tables, buf, offset)
            }
            "WDAT" => {
                handle_read_table!(WDAT, name_str, tables, buf, offset)
            }
            "WDRT" => {
                handle_read_table!(WDRT, name_str, tables, buf, offset)
            }
            "WPBT" => {
                handle_read_table!(WPBT, name_str, tables, buf, offset)
            }
            "WSMT" => {
                handle_read_table!(WSMT, name_str, tables, buf, offset)
            }
            "XENV" => {
                handle_read_table!(XENV, name_str, tables, buf, offset)
            }
            _ => Err(SystemError::ENODEV),
        }
    }
}

impl Attribute for AttrAcpiTable {
    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    fn store(&self, _kobj: Arc<dyn KObject>, _buf: &[u8]) -> Result<usize, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    fn name(&self) -> &str {
        return &self.filename;
    }

    fn mode(&self) -> ModeType {
        return ModeType::from_bits_truncate(0o400);
    }

    fn support(&self) -> SysFSOpsSupport {
        return SysFSOpsSupport::empty();
    }
}

impl BinAttribute for AttrAcpiTable {
    fn support_battr(&self) -> SysFSOpsSupport {
        return SysFSOpsSupport::BATTR_READ;
    }
    fn write(
        &self,
        _kobj: Arc<dyn KObject>,
        _buf: &[u8],
        _offset: usize,
    ) -> Result<usize, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    /// 展示 ACPI 表的内容
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/acpi/sysfs.c?fi=acpi_sysfs_init#320
    fn read(
        &self,
        _kobj: Arc<dyn KObject>,
        buf: &mut [u8],
        offset: usize,
    ) -> Result<usize, SystemError> {
        let tables = acpi_manager().tables().unwrap();
        let name_str = self.name.as_str();
        // 这里分多个函数进行处理，是为了减小栈内存的使用。
        if let Ok(x) = self.do_binattr_read_1(tables, name_str, buf, offset) {
            return Ok(x);
        }

        if let Ok(x) = self.do_binattr_read_2(tables, name_str, buf, offset) {
            return Ok(x);
        }

        error!("AttrAcpiTable::read(): unknown table. name: {}", self.name);
        return Err(SystemError::ENODEV);
    }

    fn size(&self) -> usize {
        return self.size;
    }
}
