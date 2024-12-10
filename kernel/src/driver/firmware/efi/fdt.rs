//! 与UEFI相关的fdt操作

use core::fmt::Debug;

use fdt::Fdt;
use log::error;
use system_error::SystemError;

use crate::init::boot_params;

use super::EFIManager;

// 由于代码涉及转换，因此这里每个的大小都是8字节
#[derive(Default)]
pub struct EFIFdtParams {
    // systable
    pub systable: Option<u64>,
    // mmap_base
    pub mmap_base: Option<u64>,
    // mmap_size
    pub mmap_size: Option<u64>,
    // mmap_desc_size
    pub mmap_desc_size: Option<u64>,
    // mmap_desc_version
    pub mmap_desc_version: Option<u64>,
}

impl Debug for EFIFdtParams {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // 十六进制输出
        f.debug_struct("EFIFdtParams")
            .field("systable", &format_args!("{:#x?}", self.systable))
            .field("mmap_base", &format_args!("{:#x?}", self.mmap_base))
            .field("mmap_size", &format_args!("{:#x?}", self.mmap_size))
            .field(
                "mmap_desc_size",
                &format_args!("{:#x?}", self.mmap_desc_size),
            )
            .field(
                "mmap_desc_version",
                &format_args!("{:#x?}", self.mmap_desc_version),
            )
            .finish()
    }
}

/// 要从FDT中获取的属性
#[derive(Debug, Clone, Copy)]
enum FdtPropType {
    SystemTable,
    MMBase,
    MMSize,
    DescSize,
    DescVersion,
}

impl FdtPropType {
    /// 获取属性对应的fdt属性名
    fn prop_name(&self) -> &'static str {
        (*self).into()
    }
}

impl From<FdtPropType> for &'static str {
    fn from(value: FdtPropType) -> Self {
        match value {
            FdtPropType::SystemTable => "linux,uefi-system-table",
            FdtPropType::MMBase => "linux,uefi-mmap-start",
            FdtPropType::MMSize => "linux,uefi-mmap-size",
            FdtPropType::DescSize => "linux,uefi-mmap-desc-size",
            FdtPropType::DescVersion => "linux,uefi-mmap-desc-ver",
        }
    }
}

impl TryFrom<&str> for FdtPropType {
    type Error = SystemError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "linux,uefi-system-table" => Ok(FdtPropType::SystemTable),
            "linux,uefi-mmap-start" => Ok(FdtPropType::MMBase),
            "linux,uefi-mmap-size" => Ok(FdtPropType::MMSize),
            "linux,uefi-mmap-desc-size" => Ok(FdtPropType::DescSize),
            "linux,uefi-mmap-desc-ver" => Ok(FdtPropType::DescVersion),
            _ => Err(SystemError::EINVAL),
        }
    }
}

struct ParamToRead {
    /// FDT节点路径
    path: &'static str,
    /// 当前节点下要读取的属性
    properties: &'static [FdtPropType],
}

static PARAM_TO_READ: &[ParamToRead] = &[ParamToRead {
    path: "/chosen",
    properties: &[
        FdtPropType::SystemTable,
        FdtPropType::MMBase,
        FdtPropType::MMSize,
        FdtPropType::DescSize,
        FdtPropType::DescVersion,
    ],
}];

impl EFIManager {
    pub(super) fn get_fdt_params(&self) -> Result<EFIFdtParams, SystemError> {
        let fdt = unsafe {
            Fdt::from_ptr(
                boot_params()
                    .read()
                    .fdt()
                    .ok_or(SystemError::ENODEV)?
                    .data() as *const u8,
            )
        }
        .map_err(|e| {
            error!("failed to parse fdt, err={:?}", e);
            SystemError::EINVAL
        })?;

        let mut ret = EFIFdtParams::default();

        for param in PARAM_TO_READ {
            let node = fdt.find_node(param.path);
            if node.is_none() {
                continue;
            }
            let node = node.unwrap();

            for prop in param.properties {
                let prop = node.property(prop.prop_name());
                if prop.is_none() {
                    continue;
                }
                let prop = prop.unwrap();
                let prop_type = FdtPropType::try_from(prop.name);
                if prop_type.is_err() {
                    continue;
                }

                let prop_type = prop_type.unwrap();

                self.do_get_fdt_prop(prop_type, &prop, &mut ret)
                    .unwrap_or_else(|e| {
                        error!("Failed to get fdt prop: {prop_type:?}, error: {e:?}");
                    })
            }
        }

        return Ok(ret);
    }

    fn do_get_fdt_prop(
        &self,
        prop_type: FdtPropType,
        prop: &fdt::node::NodeProperty<'_>,
        target: &mut EFIFdtParams,
    ) -> Result<(), SystemError> {
        let val = if prop.value.len() == 4 {
            u32::from_be_bytes(prop.value[0..4].try_into().unwrap()) as u64
        } else {
            u64::from_be_bytes(prop.value[0..8].try_into().unwrap())
        };

        match prop_type {
            FdtPropType::SystemTable => {
                target.systable = Some(val);
            }
            FdtPropType::MMBase => {
                target.mmap_base = Some(val);
            }
            FdtPropType::MMSize => {
                target.mmap_size = Some(val);
            }
            FdtPropType::DescSize => {
                target.mmap_desc_size = Some(val);
            }
            FdtPropType::DescVersion => {
                target.mmap_desc_version = Some(val);
            }
        }

        return Ok(());
    }
}
