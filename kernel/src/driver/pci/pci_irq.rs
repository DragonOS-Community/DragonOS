#![allow(dead_code)]

use core::mem::size_of;
use core::ptr::NonNull;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use system_error::SystemError;

use super::pci::{PciDeviceStructure, PciDeviceStructureGeneralDevice, PciError};
use crate::arch::msi::{arch_msi_message_address, arch_msi_message_data};
use crate::arch::{PciArch, TraitPciArch};

use crate::driver::base::device::DeviceId;
use crate::exception::irqdesc::{IrqHandleFlags, IrqHandler};
use crate::exception::manage::irq_manager;
use crate::exception::IrqNumber;
use crate::libs::volatile::{volread, volwrite, Volatile};

/// MSIX表的一项
#[repr(C)]
struct MsixEntry {
    msg_addr: Volatile<u32>,
    msg_upper_addr: Volatile<u32>,
    msg_data: Volatile<u32>,
    vector_control: Volatile<u32>,
}

/// Pending表的一项
#[repr(C)]
struct PendingEntry {
    entry: Volatile<u64>,
}

/// PCI设备中断错误
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PciIrqError {
    IrqTypeNotSupported,
    PciDeviceNotSupportIrq,
    IrqTypeUnmatch,
    InvalidIrqIndex(u16),
    InvalidIrqNum(IrqNumber),
    IrqNumOccupied(IrqNumber),
    DeviceIrqOverflow,
    MxiIrqNumWrong,
    PciBarNotInited,
    BarGetVaddrFailed,
    MaskNotSupported,
    IrqNotInited,
}

/// PCI设备的中断类型
#[derive(Copy, Clone, Debug)]
pub enum IrqType {
    Msi {
        address_64: bool,
        maskable: bool,
        irq_max_num: u16,
        cap_offset: u8,
    },
    Msix {
        msix_table_bar: u8,
        msix_table_offset: u32,
        pending_table_bar: u8,
        pending_table_offset: u32,
        irq_max_num: u16,
        cap_offset: u8,
    },
    Legacy,
    Unused,
}

// PCI设备install中断时需要传递的参数
#[derive(Clone, Debug)]
pub struct PciIrqMsg {
    pub irq_common_message: IrqCommonMsg,
    pub irq_specific_message: IrqSpecificMsg,
}

// PCI设备install中断时需要传递的共同参数
#[derive(Clone, Debug)]
pub struct IrqCommonMsg {
    irq_index: u16,                      //要install的中断号在PCI设备中的irq_vector的index
    irq_name: String,                    //中断名字
    irq_hander: &'static dyn IrqHandler, // 中断处理函数
    /// 全局设备标志符
    dev_id: Arc<DeviceId>,
}

impl IrqCommonMsg {
    pub fn init_from(
        irq_index: u16,
        irq_name: String,
        irq_hander: &'static dyn IrqHandler,
        dev_id: Arc<DeviceId>,
    ) -> Self {
        IrqCommonMsg {
            irq_index,
            irq_name,
            irq_hander,
            dev_id,
        }
    }

    pub fn set_handler(&mut self, irq_hander: &'static dyn IrqHandler) {
        self.irq_hander = irq_hander;
    }

    pub fn dev_id(&self) -> &Arc<DeviceId> {
        &self.dev_id
    }
}

// PCI设备install中断时需要传递的特有参数，Msi代表MSI与MSIX
#[derive(Clone, Debug)]
pub enum IrqSpecificMsg {
    Legacy,
    Msi {
        processor: u16,
        trigger_mode: TriggerMode,
    },
}
impl IrqSpecificMsg {
    pub fn msi_default() -> Self {
        IrqSpecificMsg::Msi {
            processor: 0,
            trigger_mode: TriggerMode::EdgeTrigger,
        }
    }
}

// 申请中断的触发模式，MSI默认为边沿触发
#[derive(Copy, Clone, Debug)]
pub enum TriggerMode {
    EdgeTrigger,
    AssertHigh,
    AssertLow,
}

bitflags! {
    /// 设备中断类型，使用bitflag使得中断类型的选择更多元化
    pub struct IRQ: u8{
        const PCI_IRQ_LEGACY = 1 << 0;
        const PCI_IRQ_MSI = 1 << 1;
        const PCI_IRQ_MSIX = 1 << 2;
        const PCI_IRQ_ALL_TYPES=IRQ::PCI_IRQ_LEGACY.bits|IRQ::PCI_IRQ_MSI.bits|IRQ::PCI_IRQ_MSIX.bits;
    }
}

/// PciDeviceStructure的子trait，使用继承以直接使用PciDeviceStructure里的接口
pub trait PciInterrupt: PciDeviceStructure {
    /// @brief PCI设备调用该函数选择中断类型
    /// @param self PCI设备的可变引用
    /// @param flag 选择的中断类型（支持多个选择），如PCI_IRQ_ALL_TYPES表示所有中断类型均可，让系统按顺序进行选择
    /// @return Option<IrqType> 失败返回None，成功则返回对应中断类型
    fn irq_init(&mut self, flag: IRQ) -> Option<IrqType> {
        // MSIX中断优先
        if flag.contains(IRQ::PCI_IRQ_MSIX) {
            if let Some(cap_offset) = self.msix_capability_offset() {
                let data =
                    PciArch::read_config(&self.common_header().bus_device_function, cap_offset);
                let irq_max_num = ((data >> 16) & 0x7ff) as u16 + 1;
                let data =
                    PciArch::read_config(&self.common_header().bus_device_function, cap_offset + 4);
                let msix_table_bar = (data & 0x07) as u8;
                let msix_table_offset = data & (!0x07);
                let data =
                    PciArch::read_config(&self.common_header().bus_device_function, cap_offset + 8);
                let pending_table_bar = (data & 0x07) as u8;
                let pending_table_offset = data & (!0x07);
                *self.irq_type_mut()? = IrqType::Msix {
                    msix_table_bar,
                    msix_table_offset,
                    pending_table_bar,
                    pending_table_offset,
                    irq_max_num,
                    cap_offset,
                };
                return Some(IrqType::Msix {
                    msix_table_bar,
                    msix_table_offset,
                    pending_table_bar,
                    pending_table_offset,
                    irq_max_num,
                    cap_offset,
                });
            }
        }
        // 其次MSI
        if flag.contains(IRQ::PCI_IRQ_MSI) {
            if let Some(cap_offset) = self.msi_capability_offset() {
                let data =
                    PciArch::read_config(&self.common_header().bus_device_function, cap_offset);
                let message_control = (data >> 16) as u16;
                let maskable = (message_control & 0x0100) != 0;
                let address_64 = (message_control & 0x0080) != 0;
                let irq_max_num = (1 << (((message_control & 0x000e) >> 1) + 1)) as u16;
                *self.irq_type_mut()? = IrqType::Msi {
                    address_64,
                    maskable,
                    irq_max_num,
                    cap_offset,
                };
                return Some(IrqType::Msi {
                    address_64,
                    maskable,
                    irq_max_num,
                    cap_offset,
                });
            }
        }
        // 最后选择legacy#
        if flag.contains(IRQ::PCI_IRQ_LEGACY) {
            *self.irq_type_mut()? = IrqType::Legacy;
            return Some(IrqType::Legacy);
        }
        None
    }

    /// @brief 启动/关闭设备中断
    /// @param self PCI设备的可变引用
    /// @param enable 开启/关闭
    fn irq_enable(&mut self, enable: bool) -> Result<u8, PciError> {
        if let Some(irq_type) = self.irq_type_mut() {
            match *irq_type {
                IrqType::Msix { .. } => {
                    return self.msix_enable(enable);
                }
                IrqType::Msi { .. } => {
                    return self.msi_enable(enable);
                }
                IrqType::Legacy => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqTypeNotSupported));
                }
                IrqType::Unused => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqNotInited));
                }
            }
        }
        return Err(PciError::PciIrqError(PciIrqError::PciDeviceNotSupportIrq));
    }
    /// @brief 启动/关闭设备MSIX中断
    /// @param self PCI设备的可变引用
    /// @param enable 开启/关闭
    fn msix_enable(&mut self, enable: bool) -> Result<u8, PciError> {
        if let Some(irq_type) = self.irq_type_mut() {
            match *irq_type {
                IrqType::Msix { cap_offset, .. } => {
                    let mut message =
                        PciArch::read_config(&self.common_header().bus_device_function, cap_offset);
                    if enable {
                        message |= 1 << 31;
                    } else {
                        message &= !(1 << 31);
                    }
                    PciArch::write_config(
                        &self.common_header().bus_device_function,
                        cap_offset,
                        message,
                    );
                    return Ok(0);
                }
                IrqType::Unused => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqNotInited));
                }
                _ => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqTypeUnmatch));
                }
            }
        }
        return Err(PciError::PciIrqError(PciIrqError::PciDeviceNotSupportIrq));
    }
    /// @brief 启动/关闭设备MSI中断
    /// @param self PCI设备的可变引用
    /// @param enable 开启/关闭
    fn msi_enable(&mut self, enable: bool) -> Result<u8, PciError> {
        if let Some(irq_type) = self.irq_type_mut() {
            match *irq_type {
                IrqType::Msi { cap_offset, .. } => {
                    let mut message =
                        PciArch::read_config(&self.common_header().bus_device_function, cap_offset);
                    if enable {
                        message |= 1 << 16;
                    } else {
                        message &= !(1 << 16);
                    }
                    PciArch::write_config(
                        &self.common_header().bus_device_function,
                        cap_offset,
                        message,
                    );
                    return Ok(0);
                }
                IrqType::Unused => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqNotInited));
                }
                _ => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqTypeUnmatch));
                }
            }
        }
        return Err(PciError::PciIrqError(PciIrqError::PciDeviceNotSupportIrq));
    }
    /// @brief 获取指定数量的中断号 todo 需要中断重构支持
    fn irq_alloc(_num: u16) -> Option<Vec<u16>> {
        None
    }
    /// @brief 进行PCI设备中断的安装
    /// @param self PCI设备的可变引用
    /// @param msg PCI设备install中断时需要传递的共同参数
    /// @return 一切正常返回Ok(0),有错误返回对应错误原因
    fn irq_install(&mut self, msg: PciIrqMsg) -> Result<u8, PciError> {
        if let Some(irq_vector) = self.irq_vector_mut() {
            if msg.irq_common_message.irq_index as usize > irq_vector.len() {
                return Err(PciError::PciIrqError(PciIrqError::InvalidIrqIndex(
                    msg.irq_common_message.irq_index,
                )));
            }
        }
        self.irq_enable(false)?; //中断设置更改前先关闭对应PCI设备的中断
        if let Some(irq_type) = self.irq_type_mut() {
            match *irq_type {
                IrqType::Msix { .. } => {
                    return self.msix_install(msg);
                }
                IrqType::Msi { .. } => {
                    return self.msi_install(msg);
                }
                IrqType::Unused => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqNotInited));
                }
                _ => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqTypeNotSupported));
                }
            }
        }
        return Err(PciError::PciIrqError(PciIrqError::PciDeviceNotSupportIrq));
    }
    /// @brief 进行PCI设备中断的安装(MSI)
    /// @param self PCI设备的可变引用
    /// @param msg PCI设备install中断时需要传递的共同参数
    /// @return 一切正常返回Ok(0),有错误返回对应错误原因
    fn msi_install(&mut self, msg: PciIrqMsg) -> Result<u8, PciError> {
        if let Some(irq_type) = self.irq_type_mut() {
            match *irq_type {
                IrqType::Msi {
                    address_64,
                    irq_max_num,
                    cap_offset,
                    ..
                } => {
                    // 注意：MSI中断分配的中断号必须连续且大小为2的倍数
                    if self.irq_vector_mut().unwrap().len() > irq_max_num as usize {
                        return Err(PciError::PciIrqError(PciIrqError::DeviceIrqOverflow));
                    }
                    let irq_num =
                        self.irq_vector_mut().unwrap()[msg.irq_common_message.irq_index as usize];

                    let irq_num = IrqNumber::new(irq_num.into());
                    let common_msg = &msg.irq_common_message;

                    let result = irq_manager().request_irq(
                        irq_num,
                        common_msg.irq_name.clone(),
                        common_msg.irq_hander,
                        IrqHandleFlags::empty(),
                        Some(common_msg.dev_id.clone()),
                    );

                    match result {
                        Ok(_) => {}
                        Err(SystemError::EINVAL) => {
                            return Err(PciError::PciIrqError(PciIrqError::InvalidIrqNum(irq_num)));
                        }

                        Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                            return Err(PciError::PciIrqError(PciIrqError::IrqNumOccupied(
                                irq_num,
                            )));
                        }

                        Err(_) => {
                            kerror!(
                                "Failed to request pci irq {} for device {}",
                                irq_num.data(),
                                &common_msg.irq_name
                            );
                            return Err(PciError::PciIrqError(PciIrqError::IrqNumOccupied(
                                irq_num,
                            )));
                        }
                    }

                    // MSI中断只需配置一次PCI寄存器
                    if common_msg.irq_index == 0 {
                        let msg_address = arch_msi_message_address(0);
                        let trigger = match msg.irq_specific_message {
                            IrqSpecificMsg::Legacy => {
                                return Err(PciError::PciIrqError(PciIrqError::IrqTypeUnmatch));
                            }
                            IrqSpecificMsg::Msi { trigger_mode, .. } => trigger_mode,
                        };
                        let msg_data = arch_msi_message_data(irq_num.data() as u16, 0, trigger);
                        // 写入Message Data和Message Address
                        if address_64 {
                            PciArch::write_config(
                                &self.common_header().bus_device_function,
                                cap_offset + 4,
                                msg_address,
                            );
                            PciArch::write_config(
                                &self.common_header().bus_device_function,
                                cap_offset + 8,
                                0,
                            );
                            PciArch::write_config(
                                &self.common_header().bus_device_function,
                                cap_offset + 12,
                                msg_data,
                            );
                        } else {
                            PciArch::write_config(
                                &self.common_header().bus_device_function,
                                cap_offset + 4,
                                msg_address,
                            );
                            PciArch::write_config(
                                &self.common_header().bus_device_function,
                                cap_offset + 8,
                                msg_data,
                            );
                        }
                        let data = PciArch::read_config(
                            &self.common_header().bus_device_function,
                            cap_offset,
                        );
                        let message_control = (data >> 16) as u16;
                        match self.irq_vector_mut().unwrap().len() {
                            1 => {
                                let temp = message_control & (!0x0070);
                                PciArch::write_config(
                                    &self.common_header().bus_device_function,
                                    cap_offset,
                                    (temp as u32) << 16,
                                );
                            }
                            2 => {
                                let temp = message_control & (!0x0070);
                                PciArch::write_config(
                                    &self.common_header().bus_device_function,
                                    cap_offset,
                                    ((temp | (0x0001 << 4)) as u32) << 16,
                                );
                            }
                            4 => {
                                let temp = message_control & (!0x0070);
                                PciArch::write_config(
                                    &self.common_header().bus_device_function,
                                    cap_offset,
                                    ((temp | (0x0002 << 4)) as u32) << 16,
                                );
                            }
                            8 => {
                                let temp = message_control & (!0x0070);
                                PciArch::write_config(
                                    &self.common_header().bus_device_function,
                                    cap_offset,
                                    ((temp | (0x0003 << 4)) as u32) << 16,
                                );
                            }
                            16 => {
                                let temp = message_control & (!0x0070);
                                PciArch::write_config(
                                    &self.common_header().bus_device_function,
                                    cap_offset,
                                    ((temp | (0x0004 << 4)) as u32) << 16,
                                );
                            }
                            32 => {
                                let temp = message_control & (!0x0070);
                                PciArch::write_config(
                                    &self.common_header().bus_device_function,
                                    cap_offset,
                                    ((temp | (0x0005 << 4)) as u32) << 16,
                                );
                            }
                            _ => {
                                return Err(PciError::PciIrqError(PciIrqError::MxiIrqNumWrong));
                            }
                        }
                    }
                    return Ok(0);
                }
                IrqType::Unused => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqNotInited));
                }
                _ => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqTypeUnmatch));
                }
            }
        }
        return Err(PciError::PciIrqError(PciIrqError::PciDeviceNotSupportIrq));
    }
    /// @brief 进行PCI设备中断的安装(MSIX)
    /// @param self PCI设备的可变引用
    /// @param msg PCI设备install中断时需要传递的共同参数
    /// @return 一切正常返回Ok(0),有错误返回对应错误原因
    fn msix_install(&mut self, msg: PciIrqMsg) -> Result<u8, PciError> {
        if let Some(irq_type) = self.irq_type_mut() {
            match *irq_type {
                IrqType::Msix {
                    irq_max_num,
                    msix_table_bar,
                    msix_table_offset,
                    ..
                } => {
                    if self.irq_vector_mut().unwrap().len() > irq_max_num as usize {
                        return Err(PciError::PciIrqError(PciIrqError::DeviceIrqOverflow));
                    }
                    let irq_num =
                        self.irq_vector_mut().unwrap()[msg.irq_common_message.irq_index as usize];

                    let common_msg = &msg.irq_common_message;

                    let result = irq_manager().request_irq(
                        irq_num,
                        common_msg.irq_name.clone(),
                        common_msg.irq_hander,
                        IrqHandleFlags::empty(),
                        Some(common_msg.dev_id.clone()),
                    );

                    match result {
                        Ok(_) => {}
                        Err(SystemError::EINVAL) => {
                            return Err(PciError::PciIrqError(PciIrqError::InvalidIrqNum(irq_num)));
                        }

                        Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                            return Err(PciError::PciIrqError(PciIrqError::IrqNumOccupied(
                                irq_num,
                            )));
                        }

                        Err(_) => {
                            kerror!(
                                "Failed to request pci irq {} for device {}",
                                irq_num.data(),
                                &common_msg.irq_name
                            );
                            return Err(PciError::PciIrqError(PciIrqError::IrqNumOccupied(
                                irq_num,
                            )));
                        }
                    }

                    let msg_address = arch_msi_message_address(0);
                    let trigger = match msg.irq_specific_message {
                        IrqSpecificMsg::Legacy => {
                            return Err(PciError::PciIrqError(PciIrqError::IrqTypeUnmatch));
                        }
                        IrqSpecificMsg::Msi { trigger_mode, .. } => trigger_mode,
                    };
                    let msg_data = arch_msi_message_data(irq_num.data() as u16, 0, trigger);
                    //写入Message Data和Message Address
                    let pcistandardbar = self
                        .bar()
                        .ok_or(PciError::PciIrqError(PciIrqError::PciBarNotInited))?;
                    let msix_bar = pcistandardbar.get_bar(msix_table_bar)?;
                    let vaddr: crate::mm::VirtAddr = msix_bar
                        .virtual_address()
                        .ok_or(PciError::PciIrqError(PciIrqError::BarGetVaddrFailed))?
                        + msix_table_offset as usize
                        + msg.irq_common_message.irq_index as usize * size_of::<MsixEntry>();
                    let msix_entry = NonNull::new(vaddr.data() as *mut MsixEntry).unwrap();
                    // 这里的操作并不适用于所有架构，需要再优化，msg_upper_data并不一定为0
                    unsafe {
                        volwrite!(msix_entry, vector_control, 0);
                        volwrite!(msix_entry, msg_data, msg_data);
                        volwrite!(msix_entry, msg_upper_addr, 0);
                        volwrite!(msix_entry, msg_addr, msg_address);
                    }
                    return Ok(0);
                }
                IrqType::Unused => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqNotInited));
                }
                _ => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqTypeUnmatch));
                }
            }
        }
        return Err(PciError::PciIrqError(PciIrqError::PciDeviceNotSupportIrq));
    }
    /// @brief 进行PCI设备中断的卸载
    /// @param self PCI设备的可变引用
    fn irq_uninstall(&mut self) -> Result<u8, PciError> {
        self.irq_enable(false)?; //中断设置更改前先关闭对应PCI设备的中断
        if let Some(irq_type) = self.irq_type_mut() {
            match *irq_type {
                IrqType::Msix { .. } => {
                    return self.msix_uninstall();
                }
                IrqType::Msi { .. } => {
                    return self.msi_uninstall();
                }
                IrqType::Unused => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqNotInited));
                }
                _ => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqTypeNotSupported));
                }
            }
        }
        return Err(PciError::PciIrqError(PciIrqError::PciDeviceNotSupportIrq));
    }
    /// @brief 进行PCI设备中断的卸载（MSI）
    /// @param self PCI设备的可变引用
    fn msi_uninstall(&mut self) -> Result<u8, PciError> {
        if let Some(irq_type) = self.irq_type_mut() {
            match *irq_type {
                IrqType::Msi {
                    address_64,
                    cap_offset,
                    ..
                } => {
                    for vector in self.irq_vector_mut().unwrap() {
                        let irq = IrqNumber::new((*vector).into());
                        irq_manager().free_irq(irq, None);
                    }
                    PciArch::write_config(&self.common_header().bus_device_function, cap_offset, 0);
                    PciArch::write_config(
                        &self.common_header().bus_device_function,
                        cap_offset + 4,
                        0,
                    );
                    PciArch::write_config(
                        &self.common_header().bus_device_function,
                        cap_offset + 8,
                        0,
                    );
                    if address_64 {
                        PciArch::write_config(
                            &self.common_header().bus_device_function,
                            cap_offset + 12,
                            0,
                        );
                    }
                    return Ok(0);
                }
                IrqType::Unused => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqNotInited));
                }
                _ => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqTypeUnmatch));
                }
            }
        }
        return Err(PciError::PciIrqError(PciIrqError::PciDeviceNotSupportIrq));
    }
    /// @brief 进行PCI设备中断的卸载(MSIX)
    /// @param self PCI设备的可变引用
    fn msix_uninstall(&mut self) -> Result<u8, PciError> {
        if let Some(irq_type) = self.irq_type_mut() {
            match *irq_type {
                IrqType::Msix {
                    irq_max_num,
                    cap_offset,
                    msix_table_bar,
                    msix_table_offset,
                    ..
                } => {
                    for vector in self.irq_vector_mut().unwrap() {
                        let irq = IrqNumber::new((*vector).into());
                        irq_manager().free_irq(irq, None);
                    }
                    PciArch::write_config(&self.common_header().bus_device_function, cap_offset, 0);
                    let pcistandardbar = self
                        .bar()
                        .ok_or(PciError::PciIrqError(PciIrqError::PciBarNotInited))
                        .unwrap();
                    let msix_bar = pcistandardbar.get_bar(msix_table_bar).unwrap();
                    for index in 0..irq_max_num {
                        let vaddr = msix_bar
                            .virtual_address()
                            .ok_or(PciError::PciIrqError(PciIrqError::BarGetVaddrFailed))
                            .unwrap()
                            + msix_table_offset as usize
                            + index as usize * size_of::<MsixEntry>();
                        let msix_entry = NonNull::new(vaddr.data() as *mut MsixEntry).unwrap();
                        unsafe {
                            volwrite!(msix_entry, vector_control, 0);
                            volwrite!(msix_entry, msg_data, 0);
                            volwrite!(msix_entry, msg_upper_addr, 0);
                            volwrite!(msix_entry, msg_addr, 0);
                        }
                    }
                    return Ok(0);
                }
                IrqType::Unused => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqNotInited));
                }
                _ => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqTypeUnmatch));
                }
            }
        }
        return Err(PciError::PciIrqError(PciIrqError::PciDeviceNotSupportIrq));
    }
    /// @brief 屏蔽相应位置的中断
    /// @param self PCI设备的可变引用
    /// @param irq_index 中断的位置（在vec中的index和安装的index相同）
    fn irq_mask(&mut self, irq_index: u16) -> Result<u8, PciError> {
        if let Some(irq_type) = self.irq_type_mut() {
            match *irq_type {
                IrqType::Msix { .. } => {
                    return self.msix_mask(irq_index);
                }
                IrqType::Msi { .. } => {
                    return self.msi_mask(irq_index);
                }
                IrqType::Unused => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqNotInited));
                }
                _ => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqTypeNotSupported));
                }
            }
        }
        return Err(PciError::PciIrqError(PciIrqError::PciDeviceNotSupportIrq));
    }
    /// @brief 屏蔽相应位置的中断(MSI)
    /// @param self PCI设备的可变引用
    /// @param irq_index 中断的位置（在vec中的index和安装的index相同）
    fn msi_mask(&mut self, irq_index: u16) -> Result<u8, PciError> {
        if let Some(irq_type) = self.irq_type_mut() {
            match *irq_type {
                IrqType::Msi {
                    maskable,
                    address_64,
                    cap_offset,
                    irq_max_num,
                } => {
                    if irq_index >= irq_max_num {
                        return Err(PciError::PciIrqError(PciIrqError::InvalidIrqIndex(
                            irq_index,
                        )));
                    }
                    if maskable {
                        match address_64 {
                            true => {
                                let mut mask = PciArch::read_config(
                                    &self.common_header().bus_device_function,
                                    cap_offset + 16,
                                );
                                mask |= 1 << irq_index;
                                PciArch::write_config(
                                    &self.common_header().bus_device_function,
                                    cap_offset,
                                    mask,
                                );
                            }
                            false => {
                                let mut mask = PciArch::read_config(
                                    &self.common_header().bus_device_function,
                                    cap_offset + 12,
                                );
                                mask |= 1 << irq_index;
                                PciArch::write_config(
                                    &self.common_header().bus_device_function,
                                    cap_offset,
                                    mask,
                                );
                            }
                        }
                        return Ok(0);
                    }
                    return Err(PciError::PciIrqError(PciIrqError::MaskNotSupported));
                }
                IrqType::Unused => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqNotInited));
                }
                _ => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqTypeUnmatch));
                }
            }
        }
        return Err(PciError::PciIrqError(PciIrqError::PciDeviceNotSupportIrq));
    }
    /// @brief 屏蔽相应位置的中断(MSIX)
    /// @param self PCI设备的可变引用
    /// @param irq_index 中断的位置（在vec中的index和安装的index相同）
    fn msix_mask(&mut self, irq_index: u16) -> Result<u8, PciError> {
        if let Some(irq_type) = self.irq_type_mut() {
            match *irq_type {
                IrqType::Msix {
                    irq_max_num,
                    msix_table_bar,
                    msix_table_offset,
                    ..
                } => {
                    if irq_index >= irq_max_num {
                        return Err(PciError::PciIrqError(PciIrqError::InvalidIrqIndex(
                            irq_index,
                        )));
                    }
                    let pcistandardbar = self
                        .bar()
                        .ok_or(PciError::PciIrqError(PciIrqError::PciBarNotInited))
                        .unwrap();
                    let msix_bar = pcistandardbar.get_bar(msix_table_bar).unwrap();
                    let vaddr = msix_bar.virtual_address().unwrap()
                        + msix_table_offset as usize
                        + irq_index as usize * size_of::<MsixEntry>();
                    let msix_entry = NonNull::new(vaddr.data() as *mut MsixEntry).unwrap();
                    unsafe {
                        volwrite!(msix_entry, vector_control, 1);
                    }
                    return Ok(0);
                }
                IrqType::Unused => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqNotInited));
                }
                _ => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqTypeUnmatch));
                }
            }
        }
        return Err(PciError::PciIrqError(PciIrqError::PciDeviceNotSupportIrq));
    }
    /// @brief 解除屏蔽相应位置的中断
    /// @param self PCI设备的可变引用
    /// @param irq_index 中断的位置（在vec中的index和安装的index相同）
    fn irq_unmask(&mut self, irq_index: u16) -> Result<u8, PciError> {
        if let Some(irq_type) = self.irq_type_mut() {
            match *irq_type {
                IrqType::Msix { .. } => {
                    return self.msix_unmask(irq_index);
                }
                IrqType::Msi { .. } => {
                    return self.msi_unmask(irq_index);
                }
                IrqType::Unused => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqNotInited));
                }
                _ => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqTypeNotSupported));
                }
            }
        }
        return Err(PciError::PciIrqError(PciIrqError::PciDeviceNotSupportIrq));
    }
    /// @brief 解除屏蔽相应位置的中断（MSI）
    /// @param self PCI设备的可变引用
    /// @param irq_index 中断的位置（在vec中的index和安装的index相同）
    fn msi_unmask(&mut self, irq_index: u16) -> Result<u8, PciError> {
        if let Some(irq_type) = self.irq_type_mut() {
            match *irq_type {
                IrqType::Msi {
                    maskable,
                    address_64,
                    cap_offset,
                    irq_max_num,
                } => {
                    if irq_index >= irq_max_num {
                        return Err(PciError::PciIrqError(PciIrqError::InvalidIrqIndex(
                            irq_index,
                        )));
                    }
                    if maskable {
                        match address_64 {
                            true => {
                                let mut mask = PciArch::read_config(
                                    &self.common_header().bus_device_function,
                                    cap_offset + 16,
                                );
                                mask &= !(1 << irq_index);
                                PciArch::write_config(
                                    &self.common_header().bus_device_function,
                                    cap_offset,
                                    mask,
                                );
                            }
                            false => {
                                let mut mask = PciArch::read_config(
                                    &self.common_header().bus_device_function,
                                    cap_offset + 12,
                                );
                                mask &= !(1 << irq_index);
                                PciArch::write_config(
                                    &self.common_header().bus_device_function,
                                    cap_offset,
                                    mask,
                                );
                            }
                        }
                    }
                    return Err(PciError::PciIrqError(PciIrqError::MaskNotSupported));
                }
                IrqType::Unused => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqNotInited));
                }
                _ => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqTypeUnmatch));
                }
            }
        }
        return Err(PciError::PciIrqError(PciIrqError::PciDeviceNotSupportIrq));
    }
    /// @brief 解除屏蔽相应位置的中断(MSIX)
    /// @param self PCI设备的可变引用
    /// @param irq_index 中断的位置（在vec中的index和安装的index相同）
    fn msix_unmask(&mut self, irq_index: u16) -> Result<u8, PciError> {
        if let Some(irq_type) = self.irq_type_mut() {
            match *irq_type {
                IrqType::Msix {
                    irq_max_num,
                    msix_table_bar,
                    msix_table_offset,
                    ..
                } => {
                    if irq_index >= irq_max_num {
                        return Err(PciError::PciIrqError(PciIrqError::InvalidIrqIndex(
                            irq_index,
                        )));
                    }
                    let pcistandardbar = self
                        .bar()
                        .ok_or(PciError::PciIrqError(PciIrqError::PciBarNotInited))
                        .unwrap();
                    let msix_bar = pcistandardbar.get_bar(msix_table_bar).unwrap();
                    let vaddr = msix_bar.virtual_address().unwrap()
                        + msix_table_offset as usize
                        + irq_index as usize * size_of::<MsixEntry>();
                    let msix_entry = NonNull::new(vaddr.data() as *mut MsixEntry).unwrap();
                    unsafe {
                        volwrite!(msix_entry, vector_control, 0);
                    }
                    return Ok(0);
                }
                IrqType::Unused => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqNotInited));
                }
                _ => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqTypeUnmatch));
                }
            }
        }
        return Err(PciError::PciIrqError(PciIrqError::PciDeviceNotSupportIrq));
    }
    /// @brief 检查被挂起的中断是否在挂起的时候产生了
    /// @param self PCI设备的可变引用
    /// @param irq_index 中断的位置（在vec中的index和安装的index相同）
    /// @return 是否在挂起过程中产生中断（异常情况也返回false）
    fn irq_check_pending(&mut self, irq_index: u16) -> Result<bool, PciError> {
        if let Some(irq_type) = self.irq_type_mut() {
            match *irq_type {
                IrqType::Msix { .. } => {
                    return self.msix_check_pending(irq_index);
                }
                IrqType::Msi { .. } => {
                    return self.msi_check_pending(irq_index);
                }
                IrqType::Unused => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqNotInited));
                }
                _ => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqTypeNotSupported));
                }
            }
        }
        return Err(PciError::PciIrqError(PciIrqError::PciDeviceNotSupportIrq));
    }
    /// @brief 检查被挂起的中断是否在挂起的时候产生了(MSI)
    /// @param self PCI设备的可变引用
    /// @param irq_index 中断的位置（在vec中的index和安装的index相同）
    /// @return 是否在挂起过程中产生中断（异常情况也返回false）
    fn msi_check_pending(&mut self, irq_index: u16) -> Result<bool, PciError> {
        if let Some(irq_type) = self.irq_type_mut() {
            match *irq_type {
                IrqType::Msi {
                    maskable,
                    address_64,
                    cap_offset,
                    irq_max_num,
                } => {
                    if irq_index >= irq_max_num {
                        return Err(PciError::PciIrqError(PciIrqError::InvalidIrqIndex(
                            irq_index,
                        )));
                    }
                    if maskable {
                        match address_64 {
                            true => {
                                let mut pend = PciArch::read_config(
                                    &self.common_header().bus_device_function,
                                    cap_offset + 20,
                                );
                                pend &= 1 << irq_index;
                                return Ok(pend != 0);
                            }
                            false => {
                                let mut pend = PciArch::read_config(
                                    &self.common_header().bus_device_function,
                                    cap_offset + 16,
                                );
                                pend &= 1 << irq_index;
                                return Ok(pend != 0);
                            }
                        }
                    }
                }
                IrqType::Unused => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqNotInited));
                }
                _ => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqTypeUnmatch));
                }
            }
        }
        return Err(PciError::PciIrqError(PciIrqError::PciDeviceNotSupportIrq));
    }
    /// @brief 检查被挂起的中断是否在挂起的时候产生了(MSIX)
    /// @param self PCI设备的可变引用
    /// @param irq_index 中断的位置（在vec中的index和安装的index相同）
    /// @return 是否在挂起过程中产生中断（异常情况也返回false）
    fn msix_check_pending(&mut self, irq_index: u16) -> Result<bool, PciError> {
        if let Some(irq_type) = self.irq_type_mut() {
            match *irq_type {
                IrqType::Msix {
                    irq_max_num,
                    pending_table_bar,
                    pending_table_offset,
                    ..
                } => {
                    if irq_index >= irq_max_num {
                        return Err(PciError::PciIrqError(PciIrqError::InvalidIrqIndex(
                            irq_index,
                        )));
                    }
                    let pcistandardbar = self
                        .bar()
                        .ok_or(PciError::PciIrqError(PciIrqError::PciBarNotInited))
                        .unwrap();
                    let pending_bar = pcistandardbar.get_bar(pending_table_bar).unwrap();
                    let vaddr = pending_bar.virtual_address().unwrap()
                        + pending_table_offset as usize
                        + (irq_index as usize / 64) * size_of::<PendingEntry>();
                    let pending_entry = NonNull::new(vaddr.data() as *mut PendingEntry).unwrap();
                    let pending_entry = unsafe { volread!(pending_entry, entry) };
                    return Ok(pending_entry & (1 << (irq_index as u64 % 64)) != 0);
                }
                IrqType::Unused => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqNotInited));
                }
                _ => {
                    return Err(PciError::PciIrqError(PciIrqError::IrqTypeUnmatch));
                }
            }
        }
        return Err(PciError::PciIrqError(PciIrqError::PciDeviceNotSupportIrq));
    }
}
/// PCI标准设备的msi/msix中断相关函数块
impl PciInterrupt for PciDeviceStructureGeneralDevice {}
