/*
* 模块：VecCursor
* 说明：本模块用于为数组提供游标的功能，以简化其操作。
*
* Maintainer: 龙进 <longjin@RinGoTek.cn>
*/

#![allow(dead_code)]

use alloc::vec::Vec;

use crate::{
    include::bindings::bindings::{E2BIG, EINVAL, EOVERFLOW},
    io::SeekFrom,
};

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
        return Self { data: data, pos: 0 };
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
    pub fn read_u8(&mut self) -> Result<u8, i32> {
        if self.pos >= self.data.len() {
            return Err(-(E2BIG as i32));
        }
        self.pos += 1;
        return Ok(self.data[self.pos - 1]);
    }

    /// @brief 读取一个u16的数据（小端对齐）
    pub fn read_u16(&mut self) -> Result<u16, i32> {
        if self.pos + 2 > self.data.len() {
            return Err(-(E2BIG as i32));
        }
        let mut res = 0u16;
        res |= (self.data[self.pos] as u16) & 0xff;
        self.pos += 1;
        res |= ((self.data[self.pos] as u16) & 0xff) << 8;
        self.pos += 1;

        return Ok(res);
    }

    /// @brief 读取一个u32的数据（小端对齐）
    pub fn read_u32(&mut self) -> Result<u32, i32> {
        if self.pos + 4 > self.data.len() {
            return Err(-(E2BIG as i32));
        }
        let mut res = 0u32;
        for i in 0..4 {
            res |= ((self.data[self.pos] as u32) & 0xff) << (8 * i);
            self.pos += 1;
        }

        return Ok(res);
    }

    /// @brief 读取一个u64的数据（小端对齐）
    pub fn read_u64(&mut self) -> Result<u64, i32> {
        if self.pos + 8 > self.data.len() {
            return Err(-(E2BIG as i32));
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
    pub fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), i32> {
        if self.pos + buf.len() > self.data.len() {
            return Err(-(E2BIG as i32));
        }
        buf.copy_from_slice(&self.data[self.pos..self.pos + buf.len()]);
        self.pos += buf.len();
        return Ok(());
    }

    /// @brief 调整游标的位置
    ///
    /// @param 调整的相对值
    ///
    /// @return Ok(新的游标位置) 调整成功，返回新的游标位置
    /// @return Err(-EOVERFLOW) 调整失败，游标超出正确的范围。（失败时游标位置不变）
    pub fn seek(&mut self, origin: SeekFrom) -> Result<usize, i32> {
        let pos: i64;
        match origin {
            SeekFrom::SeekSet(offset) => {
                pos = offset;
            }
            SeekFrom::SeekCurrent(offset) => {
                pos = self.pos as i64 + offset;
            }
            SeekFrom::SeekEnd(offset) => {
                pos = self.data.len() as i64 + offset;
            }
            SeekFrom::Invalid => {
                return Err(-(EINVAL as i32));
            }
        }

        if pos < 0 || pos > self.data.len() as i64 {
            return Err(-(EOVERFLOW as i32));
        }
        self.pos = pos as usize;
        return Ok(self.pos);
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
