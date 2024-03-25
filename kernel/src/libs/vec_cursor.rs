#![allow(dead_code)]

use core::mem::{size_of, size_of_val};

use alloc::vec::Vec;
use system_error::SystemError;

use crate::driver::base::block::SeekFrom;

/// @brief 本模块用于为数组提供游标的功能，以简化其操作。
#[derive(Debug)]
pub struct VecCursor {
    /// 游标管理的数据
    data: Vec<u8>,
    /// 游标的位置
    pos: usize,
}

impl VecCursor {
    /// @brief 新建一个游标
    pub fn new(data: Vec<u8>) -> Self {
        return Self { data, pos: 0 };
    }

    /// @brief 创建一个全0的cursor
    pub fn zerod(length: usize) -> Self {
        let mut result = VecCursor {
            data: Vec::new(),
            pos: 0,
        };
        result.data.resize(length, 0);
        return result;
    }

    /// @brief 获取游标管理的数据的可变引用
    pub fn get_mut(&mut self) -> &mut Vec<u8> {
        return &mut self.data;
    }

    /// @brief 获取游标管理的数据的不可变引用
    pub fn get_ref(&self) -> &Vec<u8> {
        return &self.data;
    }

    /// @brief 读取一个u8的数据（小端对齐）
    pub fn read_u8(&mut self) -> Result<u8, SystemError> {
        if self.pos >= self.data.len() {
            return Err(SystemError::E2BIG);
        }
        self.pos += 1;
        return Ok(self.data[self.pos - 1]);
    }

    /// @brief 读取一个u16的数据（小端对齐）
    pub fn read_u16(&mut self) -> Result<u16, SystemError> {
        if self.pos + 2 > self.data.len() {
            return Err(SystemError::E2BIG);
        }
        let mut res = 0u16;
        res |= (self.data[self.pos] as u16) & 0xff;
        self.pos += 1;
        res |= ((self.data[self.pos] as u16) & 0xff) << 8;
        self.pos += 1;

        return Ok(res);
    }

    /// @brief 读取一个u32的数据（小端对齐）
    pub fn read_u32(&mut self) -> Result<u32, SystemError> {
        if self.pos + 4 > self.data.len() {
            return Err(SystemError::E2BIG);
        }
        let mut res = 0u32;
        for i in 0..4 {
            res |= ((self.data[self.pos] as u32) & 0xff) << (8 * i);
            self.pos += 1;
        }

        return Ok(res);
    }

    /// @brief 读取一个u64的数据（小端对齐）
    pub fn read_u64(&mut self) -> Result<u64, SystemError> {
        if self.pos + 8 > self.data.len() {
            return Err(SystemError::E2BIG);
        }
        let mut res = 0u64;
        for i in 0..8 {
            res |= ((self.data[self.pos] as u64) & 0xff) << (8 * i);
            self.pos += 1;
        }

        return Ok(res);
    }

    /// @brief 精确读取与buf同样大小的数据。
    ///
    /// @param buf 要读取到的目标缓冲区
    ///
    /// @return Ok(()) 成功读取
    /// @retunr Err(-E2BIG) 没有这么多数据，读取失败
    pub fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), SystemError> {
        if self.pos + buf.len() > self.data.len() {
            return Err(SystemError::E2BIG);
        }
        buf.copy_from_slice(&self.data[self.pos..self.pos + buf.len()]);
        self.pos += buf.len();
        return Ok(());
    }

    /// @brief 小端对齐，读取数据到u16数组.
    ///
    /// @param buf 目标u16数组
    pub fn read_u16_into(&mut self, buf: &mut [u16]) -> Result<(), SystemError> {
        if self.pos + size_of_val(buf) > self.data.len() * size_of::<u16>() {
            return Err(SystemError::E2BIG);
        }

        for item in buf.iter_mut() {
            *item = self.read_u16()?;
        }

        return Ok(());
    }

    /// @brief 调整游标的位置
    ///
    /// @param 调整的相对值
    ///
    /// @return Ok(新的游标位置) 调整成功，返回新的游标位置
    /// @return Err(SystemError::EOVERFLOW) 调整失败，游标超出正确的范围。（失败时游标位置不变）
    pub fn seek(&mut self, origin: SeekFrom) -> Result<usize, SystemError> {
        let pos = match origin {
            SeekFrom::SeekSet(offset) => offset,
            SeekFrom::SeekCurrent(offset) => self.pos as i64 + offset,
            // 请注意，此处的offset应小于等于0，否则肯定是不合法的
            SeekFrom::SeekEnd(offset) => self.data.len() as i64 + offset,
            SeekFrom::Invalid => {
                return Err(SystemError::EINVAL);
            }
        };

        if pos < 0 || pos > self.data.len() as i64 {
            return Err(SystemError::EOVERFLOW);
        }
        self.pos = pos as usize;
        return Ok(self.pos);
    }

    /// @brief 写入一个u8的数据（小端对齐）
    pub fn write_u8(&mut self, value: u8) -> Result<u8, SystemError> {
        if self.pos >= self.data.len() {
            return Err(SystemError::E2BIG);
        }

        self.data[self.pos] = value;
        self.pos += 1;

        return Ok(value);
    }

    /// @brief 写入一个u16的数据（小端对齐）
    pub fn write_u16(&mut self, value: u16) -> Result<u16, SystemError> {
        if self.pos + 2 > self.data.len() {
            return Err(SystemError::E2BIG);
        }

        self.data[self.pos] = (value & 0xff) as u8;
        self.pos += 1;
        self.data[self.pos] = ((value >> 8) & 0xff) as u8;
        self.pos += 1;

        return Ok(value);
    }

    /// @brief 写入一个u32的数据（小端对齐）
    pub fn write_u32(&mut self, value: u32) -> Result<u32, SystemError> {
        if self.pos + 4 > self.data.len() {
            return Err(SystemError::E2BIG);
        }

        for i in 0..4 {
            self.data[self.pos] = ((value >> (i * 8)) & 0xff) as u8;
            self.pos += 1;
        }

        return Ok(value);
    }

    /// @brief 写入一个u64的数据（小端对齐）
    pub fn write_u64(&mut self, value: u64) -> Result<u64, SystemError> {
        if self.pos + 8 > self.data.len() {
            return Err(SystemError::E2BIG);
        }

        for i in 0..8 {
            self.data[self.pos] = ((value >> (i * 8)) & 0xff) as u8;
            self.pos += 1;
        }

        return Ok(value);
    }

    /// @brief 精确写入与buf同样大小的数据。
    ///
    /// @param buf 要写入到的目标缓冲区
    ///
    /// @return Ok(()) 成功写入
    /// @retunr Err(-E2BIG) 没有这么多数据，写入失败
    pub fn write_exact(&mut self, buf: &[u8]) -> Result<(), SystemError> {
        if self.pos + buf.len() > self.data.len() {
            return Err(SystemError::E2BIG);
        }

        self.data[self.pos..self.pos + buf.len()].copy_from_slice(buf);
        self.pos += buf.len();

        return Ok(());
    }

    /// @brief 获取当前的数据切片
    pub fn as_slice(&self) -> &[u8] {
        return &self.data[..];
    }

    /// @brief 获取可变数据切片
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        return &mut self.data[..];
    }

    /// @brief 获取当前游标的位置
    #[inline]
    pub fn pos(&self) -> usize {
        return self.pos;
    }

    /// @brief 获取缓冲区数据的大小
    #[inline]
    pub fn len(&self) -> usize {
        return self.data.len();
    }
}
