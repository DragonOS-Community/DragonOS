use core::mem::size_of;

const FD_SET_SIZE: usize = 1024;
const FD_SET_IDX_MASK: usize = 8 * size_of::<u64>();
const FD_SET_BIT_MASK: usize = FD_SET_IDX_MASK - 1;
const FD_SET_BTYES: usize = (FD_SET_SIZE + FD_SET_BIT_MASK) / FD_SET_IDX_MASK;

/// @brief select() 使用的文件描述符集合
pub struct FdSet([u64; FD_SET_BTYES]);

impl FdSet {
    pub fn new() -> Self {
        Self([0; FD_SET_BTYES])
    }

    /// @brief 清除 fd_set 中指定的 fd 位
    pub fn clear_fd(&mut self, fd: i32) {
        if fd >= 0 {
            let fd = fd as usize;
            let index = fd / FD_SET_IDX_MASK;
            if let Some(x) = self.0.get_mut(index) {
                *x &= !(1 << (fd & FD_SET_BIT_MASK));
            }
        }
    }

    /// @brief 设置 fd_set 中指定的 fd 位
    pub fn set_fd(&mut self, fd: i32) {
        if fd >= 0 {
            let fd = fd as usize;
            let index = fd / FD_SET_IDX_MASK;
            if let Some(x) = self.0.get_mut(index) {
                *x |= 1 << (fd & FD_SET_BIT_MASK);
            }
        }
    }

    /// @brief 判断 fd_set 中是否存在指定的 fd 位
    pub fn is_set_fd(&self, fd: i32) -> bool {
        if fd >= 0 {
            let fd = fd as usize;
            let index = fd / FD_SET_IDX_MASK;
            if let Some(x) = self.0.get(index) {
                return (*x & (1 << (fd & FD_SET_BIT_MASK))) != 0;
            }
        }
        return false;
    }

    /// @brief 清空 fd_set 中所有的 fd
    pub fn zero(&mut self) {
        for x in self.0.iter_mut() {
            *x = 0;
        }
    }
}
