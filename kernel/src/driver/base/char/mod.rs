use crate::syscall::SystemError;

use super::device::Device;

pub mod stdio;

pub trait CharDevice: Device {
    /// Notice buffer对应设备按字节划分，使用u8类型
    /// Notice offset应该从0开始计数

    /// @brief: 从设备的第offset个字节开始，读取len个byte，存放到buf中
    /// @parameter offset: 起始字节偏移量
    /// @parameter len: 读取字节的数量
    /// @parameter buf: 目标数组
    /// @return: 如果操作成功，返回操作的长度(单位是字节)；否则返回错误码；如果操作异常，但是并没有检查出什么错误，将返回已操作的长度
    fn read_at(&self, offset: usize, len: usize, buf: &mut [u8]) -> Result<usize, SystemError>;

    /// @brief: 从设备的第offset个字节开始，把buf数组的len个byte，写入到设备中
    /// @parameter offset: 起始字节偏移量
    /// @parameter len: 读取字节的数量
    /// @parameter buf: 目标数组
    /// @return: 如果操作成功，返回操作的长度(单位是字节)；否则返回错误码；如果操作异常，但是并没有检查出什么错误，将返回已操作的长度
    fn write_at(&self, offset: usize, len: usize, buf: &[u8]) -> Result<usize, SystemError>;

    /// @brief: 同步信息，把所有的dirty数据写回设备 - 待实现
    fn sync(&self) -> Result<(), SystemError>;
}
