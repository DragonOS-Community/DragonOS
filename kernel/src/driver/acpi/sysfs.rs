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
        macro_rules! copy_data {
            ($table:expr) => {
                let from = unsafe {
                    core::slice::from_raw_parts(
                        $table.virtual_start().as_ptr() as *const u8,
                        $table.region_length(),
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
            };
        }

        macro_rules! define_struct {
            ($name:ident) => {
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
            };
        }

        macro_rules! handle {
            ($name: ident, $tables: expr) => {
                define_struct!($name);
                let table = $tables.find_entire_table::<$name>().map_err(|e| {
                    warn!(
                        "AttrAcpiTable::read(): failed to find table. name: {}, error: {:?}",
                        self.name, e
                    );
                    SystemError::ENODEV
                })?;

                copy_data!(table);
            };
        }

        let tables = acpi_manager().tables().unwrap();
        match self.name.as_str() {
            "RSDT" => {
                handle!(RSDT, tables);
            }
            "XSDT" => {
                handle!(XSDT, tables);
            }
            "FACP" => {
                handle!(FADT, tables);
            }
            "HPET" => {
                handle!(HPET, tables);
            }
            "APIC" => {
                handle!(MADT, tables);
            }
            "MCFG" => {
                handle!(MCFG, tables);
            }
            "SSDT" => {
                handle!(SSDT, tables);
            }
            "BERT" => {
                handle!(BERT, tables);
            }
            "BGRT" => {
                handle!(BGRT, tables);
            }
            "CPEP" => {
                handle!(CPEP, tables);
            }
            "DSDT" => {
                handle!(DSDT, tables);
            }
            "ECDT" => {
                handle!(ECDT, tables);
            }
            "EINJ" => {
                handle!(EINJ, tables);
            }
            "ERST" => {
                handle!(ERST, tables);
            }
            "FACS" => {
                handle!(FACS, tables);
            }
            "FPDT" => {
                handle!(FPDT, tables);
            }
            "GTDT" => {
                handle!(GTDT, tables);
            }
            "HEST" => {
                handle!(HEST, tables);
            }
            "MSCT" => {
                handle!(MSCT, tables);
            }
            "MPST" => {
                handle!(MPST, tables);
            }
            "NFIT" => {
                handle!(NFIT, tables);
            }
            "PCCT" => {
                handle!(PCCT, tables);
            }
            "PHAT" => {
                handle!(PHAT, tables);
            }
            "PMTT" => {
                handle!(PMTT, tables);
            }
            "PSDT" => {
                handle!(PSDT, tables);
            }
            "RASF" => {
                handle!(RASF, tables);
            }
            "SBST" => {
                handle!(SBST, tables);
            }
            "SDEV" => {
                handle!(SDEV, tables);
            }
            "SLIT" => {
                handle!(SLIT, tables);
            }
            "SRAT" => {
                handle!(SRAT, tables);
            }
            "AEST" => {
                handle!(AEST, tables);
            }
            "BDAT" => {
                handle!(BDAT, tables);
            }
            "CDIT" => {
                handle!(CDIT, tables);
            }
            "CEDT" => {
                handle!(CEDT, tables);
            }
            "CRAT" => {
                handle!(CRAT, tables);
            }
            "CSRT" => {
                handle!(CSRT, tables);
            }
            "DBGP" => {
                handle!(DBGP, tables);
            }
            "DBG2" => {
                handle!(DBG2, tables);
            }
            "DMAR" => {
                handle!(DMAR, tables);
            }
            "DRTM" => {
                handle!(DRTM, tables);
            }
            "ETDT" => {
                handle!(ETDT, tables);
            }
            "IBFT" => {
                handle!(IBFT, tables);
            }
            "IORT" => {
                handle!(IORT, tables);
            }
            "IVRS" => {
                handle!(IVRS, tables);
            }
            "LPIT" => {
                handle!(LPIT, tables);
            }
            "MCHI" => {
                handle!(MCHI, tables);
            }
            "MPAM" => {
                handle!(MPAM, tables);
            }
            "MSDM" => {
                handle!(MSDM, tables);
            }
            "PRMT" => {
                handle!(PRMT, tables);
            }
            "RGRT" => {
                handle!(RGRT, tables);
            }
            "SDEI" => {
                handle!(SDEI, tables);
            }
            "SLIC" => {
                handle!(SLIC, tables);
            }
            "SPCR" => {
                handle!(SPCR, tables);
            }
            "SPMI" => {
                handle!(SPMI, tables);
            }
            "STAO" => {
                handle!(STAO, tables);
            }
            "SVKL" => {
                handle!(SVKL, tables);
            }
            "TCPA" => {
                handle!(TCPA, tables);
            }
            "TPM2" => {
                handle!(TPM2, tables);
            }
            "UEFI" => {
                handle!(UEFI, tables);
            }
            "WAET" => {
                handle!(WAET, tables);
            }
            "WDAT" => {
                handle!(WDAT, tables);
            }
            "WDRT" => {
                handle!(WDRT, tables);
            }
            "WPBT" => {
                handle!(WPBT, tables);
            }
            "WSMT" => {
                handle!(WSMT, tables);
            }
            "XENV" => {
                handle!(XENV, tables);
            }

            _ => {
                error!("AttrAcpiTable::read(): unknown table. name: {}", self.name);
                return Err(SystemError::ENODEV);
            }
        };
    }

    fn size(&self) -> usize {
        return self.size;
    }
}
