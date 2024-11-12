#![allow(dead_code)]
use core::{cmp::min, intrinsics::unlikely};
use log::{debug, warn};
use system_error::SystemError;

use crate::{
    driver::base::block::{block_device::LBA_SIZE, SeekFrom},
    libs::vec_cursor::VecCursor,
};
use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};

use super::{
    fs::{Cluster, FATFileSystem, MAX_FILE_SIZE},
    utils::decode_u8_ascii,
};

#[derive(Debug, Clone, Copy, Default)]
pub struct FileAttributes {
    value: u8,
}

/// FAT表中，关于每个簇的信息
#[derive(Debug, Eq, PartialEq)]
pub enum FATEntry {
    /// 当前簇未使用
    Unused,
    /// 当前簇是坏簇
    Bad,
    /// 当前簇是整个FAT簇链的最后一个簇
    EndOfChain,
    /// 在整个链中，当前簇的下一个簇的值
    Next(Cluster),
}

/// FAT目录项的枚举类型
#[derive(Debug, Clone)]
pub enum FATDirEntry {
    File(FATFile),
    VolId(FATFile),
    Dir(FATDir),
    UnInit,
}

/// FAT文件系统中的文件
#[derive(Debug, Default, Clone)]
pub struct FATFile {
    /// 文件的第一个簇
    pub first_cluster: Cluster,
    /// 文件名
    pub file_name: String,
    /// 文件对应的短目录项
    pub short_dir_entry: ShortDirEntry,
    /// 文件目录项的起始、终止簇。格式：(簇，簇内偏移量)
    pub loc: ((Cluster, u64), (Cluster, u64)),
}

impl FATFile {
    /// @brief 获取文件大小
    #[inline]
    pub fn size(&self) -> u64 {
        return self.short_dir_entry.file_size as u64;
    }

    /// @brief 设置当前文件大小（仅仅更改short_dir_entry内的值）
    #[inline]
    pub fn set_size(&mut self, size: u32) {
        self.short_dir_entry.file_size = size;
    }

    /// @brief 从文件读取数据。读取的字节数与buf长度相等
    ///
    /// @param buf 输出缓冲区
    /// @param offset 起始位置在文件中的偏移量
    ///
    /// @return Ok(usize) 成功读取到的字节数
    /// @return Err(SystemError) 读取时出现错误，返回错误码
    pub fn read(
        &self,
        fs: &Arc<FATFileSystem>,
        buf: &mut [u8],
        offset: u64,
    ) -> Result<usize, SystemError> {
        if offset >= self.size() {
            return Ok(0);
        }

        // 文件内的簇偏移量
        let start_cluster_number: u64 = offset / fs.bytes_per_cluster();
        // 计算对应在分区内的簇号
        let mut current_cluster = if let Some(c) =
            fs.get_cluster_by_relative(self.first_cluster, start_cluster_number as usize)
        {
            c
        } else {
            return Ok(0);
        };

        let bytes_remain: u64 = self.size() - offset;

        // 计算簇内偏移量
        let mut in_cluster_offset: u64 = offset % fs.bytes_per_cluster();
        let to_read_size: usize = min(buf.len(), bytes_remain as usize);

        let mut start = 0;
        let mut read_ok = 0;

        loop {
            // 当前簇已经读取完，尝试读取下一个簇
            if in_cluster_offset >= fs.bytes_per_cluster() {
                if let Ok(FATEntry::Next(c)) = fs.get_fat_entry(current_cluster) {
                    current_cluster = c;
                    in_cluster_offset %= fs.bytes_per_cluster();
                } else {
                    break;
                }
            }

            // 计算下一次读取，能够读多少字节
            let end_len: usize = min(
                to_read_size - read_ok,
                min(
                    (fs.bytes_per_cluster() - in_cluster_offset) as usize,
                    buf.len() - read_ok,
                ),
            );

            //  从磁盘上读取数据
            let offset = fs.cluster_bytes_offset(current_cluster) + in_cluster_offset;
            let r = fs
                .gendisk
                .read_at_bytes(&mut buf[start..start + end_len], offset as usize)?;

            // 更新偏移量计数信息
            read_ok += r;
            start += r;
            in_cluster_offset += r as u64;
            if read_ok == to_read_size {
                break;
            }
        }
        // todo: 更新时间信息
        return Ok(read_ok);
    }

    /// @brief 向文件写入数据。写入的字节数与buf长度相等
    ///
    /// @param buf 输入缓冲区
    /// @param offset 起始位置在文件中的偏移量
    ///
    /// @return Ok(usize) 成功写入的字节数
    /// @return Err(SystemError) 写入时出现错误，返回错误码
    pub fn write(
        &mut self,
        fs: &Arc<FATFileSystem>,
        buf: &[u8],
        offset: u64,
    ) -> Result<usize, SystemError> {
        self.ensure_len(fs, offset, buf.len() as u64)?;

        // 要写入的第一个簇的簇号
        let start_cluster_num = offset / fs.bytes_per_cluster();
        // 获取要写入的第一个簇
        let mut current_cluster: Cluster = if let Some(c) =
            fs.get_cluster_by_relative(self.first_cluster, start_cluster_num as usize)
        {
            c
        } else {
            return Ok(0);
        };

        let mut in_cluster_bytes_offset: u64 = offset % fs.bytes_per_cluster();

        let mut start: usize = 0;
        let mut write_ok: usize = 0;

        // 循环写入数据
        loop {
            if in_cluster_bytes_offset >= fs.bytes_per_cluster() {
                if let Ok(FATEntry::Next(c)) = fs.get_fat_entry(current_cluster) {
                    current_cluster = c;
                    in_cluster_bytes_offset %= fs.bytes_per_cluster();
                } else {
                    break;
                }
            }

            let end_len = min(
                (fs.bytes_per_cluster() - in_cluster_bytes_offset) as usize,
                buf.len() - write_ok,
            );

            // 计算本次写入位置在分区上的偏移量
            let offset = fs.cluster_bytes_offset(current_cluster) + in_cluster_bytes_offset;
            // 写入磁盘
            let w = fs
                .gendisk
                .write_at_bytes(&buf[start..start + end_len], offset as usize)?;

            // 更新偏移量数据
            write_ok += w;
            start += w;
            in_cluster_bytes_offset += w as u64;

            if write_ok == buf.len() {
                break;
            }
        }
        // todo: 更新时间信息
        return Ok(write_ok);
    }

    /// @brief 确保文件从指定偏移量开始，仍有长度为len的空间。
    /// 如果文件大小不够，就尝试分配更多的空间给这个文件。
    ///
    /// @param fs 当前文件所属的文件系统
    /// @param offset 起始位置在文件内的字节偏移量
    /// @param len 期待的空闲空间长度
    ///
    /// @return Ok(()) 经过操作后，offset后面具有长度至少为len的空闲空间
    /// @return Err(SystemError) 处理过程中出现了异常。
    fn ensure_len(
        &mut self,
        fs: &Arc<FATFileSystem>,
        offset: u64,
        len: u64,
    ) -> Result<(), SystemError> {
        // 文件内本身就还有空余的空间
        if offset + len <= self.size() {
            return Ok(());
        }

        // 计算文件的最后一个簇中有多少空闲空间
        let in_cluster_offset = self.size() % fs.bytes_per_cluster();
        let mut bytes_remain_in_cluster = if in_cluster_offset == 0 {
            0
        } else {
            fs.bytes_per_cluster() - in_cluster_offset
        };

        // 计算还需要申请多少空间
        let extra_bytes = min((offset + len) - self.size(), MAX_FILE_SIZE - self.size());

        // 如果文件大小为0,证明它还没有分配簇，因此分配一个簇给它
        if self.size() == 0 {
            // first_cluster应当为0,否则将产生空间泄露的错误
            assert_eq!(self.first_cluster, Cluster::default());
            self.first_cluster = fs.allocate_cluster(None)?;
            self.short_dir_entry.set_first_cluster(self.first_cluster);
            bytes_remain_in_cluster = fs.bytes_per_cluster();
        }

        // 如果还需要更多的簇
        if bytes_remain_in_cluster < extra_bytes {
            let clusters_to_allocate =
                (extra_bytes - bytes_remain_in_cluster).div_ceil(fs.bytes_per_cluster());
            let last_cluster = if let Some(c) = fs.get_last_cluster(self.first_cluster) {
                c
            } else {
                warn!("FAT: last cluster not found, File = {self:?}");
                return Err(SystemError::EINVAL);
            };
            // 申请簇
            let mut current_cluster: Cluster = last_cluster;
            for _ in 0..clusters_to_allocate {
                current_cluster = fs.allocate_cluster(Some(current_cluster))?;
            }
        }

        // 如果文件被扩展，则清空刚刚被扩展的部分的数据
        if offset > self.size() {
            // 文件内的簇偏移
            let start_cluster: u64 = self.size() / fs.bytes_per_cluster();
            let start_cluster: Cluster = fs
                .get_cluster_by_relative(self.first_cluster, start_cluster as usize)
                .unwrap();
            // 计算当前文件末尾在分区上的字节偏移量
            let start_offset: u64 =
                fs.cluster_bytes_offset(start_cluster) + self.size() % fs.bytes_per_cluster();
            // 扩展之前，最后一个簇内还剩下多少字节的空间
            let bytes_remain: u64 = fs.bytes_per_cluster() - (self.size() % fs.bytes_per_cluster());
            // 计算在扩展之后的最后一个簇内，文件的终止字节
            let cluster_offset_start = offset / fs.bytes_per_cluster();
            // 扩展后，文件的最后
            let end_cluster: Cluster = fs
                .get_cluster_by_relative(self.first_cluster, cluster_offset_start as usize)
                .unwrap();

            if start_cluster != end_cluster {
                self.zero_range(fs, start_offset, start_offset + bytes_remain)?;
            } else {
                self.zero_range(fs, start_offset, start_offset + offset - self.size())?;
            }
        }
        // 计算文件的新大小
        let new_size = self.size() + extra_bytes;
        self.set_size(new_size as u32);
        // 计算短目录项所在的位置，更新短目录项
        let short_entry_offset = fs.cluster_bytes_offset(self.loc.1 .0) + self.loc.1 .1;
        // todo: 更新时间信息
        // 把短目录项写入磁盘
        self.short_dir_entry.flush(fs, short_entry_offset)?;
        return Ok(());
    }

    /// @brief 把分区上[range_start, range_end)范围的数据清零
    ///
    /// @param range_start 分区上起始位置（单位：字节）
    /// @param range_end 分区上终止位置（单位：字节）
    fn zero_range(
        &self,
        fs: &Arc<FATFileSystem>,
        range_start: u64,
        range_end: u64,
    ) -> Result<(), SystemError> {
        if range_end <= range_start {
            return Ok(());
        }

        let zeroes: Vec<u8> = vec![0u8; (range_end - range_start) as usize];
        fs.gendisk.write_at_bytes(&zeroes, range_start as usize)?;

        return Ok(());
    }

    /// @brief 截断文件的内容，并设置新的文件大小。如果new_size大于当前文件大小，则不做操作。
    ///
    /// @param new_size 新的文件大小，如果它大于当前文件大小，则不做操作。
    ///
    /// @return Ok(()) 操作成功
    /// @return Err(SystemError) 在操作时出现错误
    pub fn truncate(&mut self, fs: &Arc<FATFileSystem>, new_size: u64) -> Result<(), SystemError> {
        if new_size >= self.size() {
            return Ok(());
        }

        let new_last_cluster = new_size.div_ceil(fs.bytes_per_cluster());
        if let Some(begin_delete) =
            fs.get_cluster_by_relative(self.first_cluster, new_last_cluster as usize)
        {
            fs.deallocate_cluster_chain(begin_delete)?;
        };

        if new_size == 0 {
            assert!(new_last_cluster == 0);
            self.short_dir_entry.set_first_cluster(Cluster::new(0));
            self.first_cluster = Cluster::new(0);
        }

        self.set_size(new_size as u32);
        // 计算短目录项在分区内的字节偏移量
        let short_entry_offset = fs.cluster_bytes_offset((self.loc.1).0) + (self.loc.1).1;
        self.short_dir_entry.flush(fs, short_entry_offset)?;

        return Ok(());
    }
}

/// FAT文件系统中的文件夹
#[derive(Debug, Default, Clone)]
pub struct FATDir {
    /// 目录的第一个簇
    pub first_cluster: Cluster,
    /// 该字段仅对FAT12、FAT16生效，表示根目录在分区内的偏移量
    pub root_offset: Option<u64>,
    /// 文件夹名称
    pub dir_name: String,
    pub short_dir_entry: Option<ShortDirEntry>,
    /// 文件的起始、终止簇。格式：(簇，簇内偏移量)
    pub loc: Option<((Cluster, u64), (Cluster, u64))>,
}

impl FATDir {
    /// @brief 获得用于遍历当前目录的迭代器
    ///
    /// @param fs 当前目录所在的文件系统
    pub fn to_iter(&self, fs: Arc<FATFileSystem>) -> FATDirIter {
        return FATDirIter {
            current_cluster: self.first_cluster,
            offset: self.root_offset.unwrap_or(0),
            is_root: self.is_root(),
            fs,
        };
    }

    /// @brief 判断当前目录是否为根目录（仅对FAT12和FAT16生效）
    #[inline]
    pub fn is_root(&self) -> bool {
        return self.root_offset.is_some();
    }

    /// @brief 获取当前目录所占用的大小
    pub fn size(&self, fs: &Arc<FATFileSystem>) -> u64 {
        return fs.num_clusters_chain(self.first_cluster) * fs.bytes_per_cluster();
    }

    /// @brief 在目录项中，寻找num_free个连续空闲目录项
    ///
    /// @param num_free 需要的空闲目录项数目.
    /// @param fs 当前文件夹属于的文件系统
    ///
    /// @return Ok(Option<(第一个符合条件的空闲目录项所在的簇，簇内偏移量))
    /// @return Err(错误码)
    pub fn find_free_entries(
        &self,
        num_free: u64,
        fs: Arc<FATFileSystem>,
    ) -> Result<Option<(Cluster, u64)>, SystemError> {
        let mut free = 0;
        let mut current_cluster: Cluster = self.first_cluster;
        let mut offset = self.root_offset.unwrap_or(0);
        // 第一个符合条件的空闲目录项
        let mut first_free: Option<(Cluster, u64)> = None;

        loop {
            // 如果当前簇没有空间了，并且当前不是FAT12和FAT16的根目录，那么就读取下一个簇。
            if offset >= fs.bytes_per_cluster() && !self.is_root() {
                // 成功读取下一个簇
                if let Ok(FATEntry::Next(c)) = fs.get_fat_entry(current_cluster) {
                    current_cluster = c;
                    // 计算簇内偏移量
                    offset %= fs.bytes_per_cluster();
                } else {
                    // 读取失败，当前已经是最后一个簇，退出循环
                    break;
                }
            }
            // 如果当前目录是FAT12和FAT16的根目录，且已经读取完，就直接返回。
            if self.is_root() && offset > fs.root_dir_end_bytes_offset().unwrap() {
                return Ok(None);
            }

            let e_offset = fs.cluster_bytes_offset(current_cluster) + offset;
            let entry: FATRawDirEntry = get_raw_dir_entry(&fs, e_offset)?;

            match entry {
                FATRawDirEntry::Free | FATRawDirEntry::FreeRest => {
                    if free == 0 {
                        first_free = Some((current_cluster, offset));
                    }

                    free += 1;
                    if free == num_free {
                        // debug!("first_free = {first_free:?}, current_free = ({current_cluster:?}, {offset})");
                        return Ok(first_free);
                    }
                }

                // 遇到一个不空闲的目录项，那么重新开始计算空闲目录项
                _ => {
                    free = 0;
                }
            }
            offset += FATRawDirEntry::DIR_ENTRY_LEN;
        }

        // 剩余的需要获取的目录项
        let remain_entries = num_free - free;

        // 计算需要申请多少个簇
        let clusters_required =
            (remain_entries * FATRawDirEntry::DIR_ENTRY_LEN).div_ceil(fs.bytes_per_cluster());
        let mut first_cluster = Cluster::default();
        let mut prev_cluster = current_cluster;
        // debug!(
        //     "clusters_required={clusters_required}, prev_cluster={prev_cluster:?}, free ={free}"
        // );
        // 申请簇
        for i in 0..clusters_required {
            let c: Cluster = fs.allocate_cluster(Some(prev_cluster))?;
            if i == 0 {
                first_cluster = c;
            }

            prev_cluster = c;
        }

        if free > 0 {
            // 空闲目录项跨越了簇，返回第一个空闲目录项
            return Ok(first_free);
        } else {
            // 空闲目录项是在全新的簇开始的
            return Ok(Some((first_cluster, 0)));
        }
    }

    /// @brief 在当前目录中寻找目录项
    ///
    /// @param name 目录项的名字
    /// @param expect_dir 该值为Some时有效。如果期待目标目录项是文件夹，那么值为Some(true), 否则为Some(false).
    /// @param short_name_gen 短目录项名称生成器
    /// @param fs 当前目录所属的文件系统
    ///
    /// @return Ok(FATDirEntry) 找到期待的目录项
    /// @return Err(SystemError) 错误码
    pub fn find_entry(
        &self,
        name: &str,
        expect_dir: Option<bool>,
        mut short_name_gen: Option<&mut ShortNameGenerator>,
        fs: Arc<FATFileSystem>,
    ) -> Result<FATDirEntry, SystemError> {
        LongDirEntry::validate_long_name(name)?;
        // 迭代当前目录下的文件/文件夹
        for e in self.to_iter(fs) {
            if e.eq_name(name) {
                if expect_dir.is_some() && Some(e.is_dir()) != expect_dir {
                    if e.is_dir() {
                        // 期望得到文件，但是是文件夹
                        return Err(SystemError::EISDIR);
                    } else {
                        // 期望得到文件夹，但是是文件
                        return Err(SystemError::ENOTDIR);
                    }
                }
                // 找到期望的目录项
                return Ok(e);
            }

            if let Some(ref mut sng) = short_name_gen {
                sng.add_name(&e.short_name_raw())
            }
        }
        // 找不到文件/文件夹
        return Err(SystemError::ENOENT);
    }

    /// @brief 在当前目录下打开文件，获取FATFile结构体
    pub fn open_file(&self, name: &str, fs: Arc<FATFileSystem>) -> Result<FATFile, SystemError> {
        let f: FATFile = self.find_entry(name, Some(false), None, fs)?.to_file()?;
        return Ok(f);
    }

    /// @brief 在当前目录下打开文件夹，获取FATDir结构体
    pub fn open_dir(&self, name: &str, fs: Arc<FATFileSystem>) -> Result<FATDir, SystemError> {
        let d: FATDir = self.find_entry(name, Some(true), None, fs)?.to_dir()?;
        return Ok(d);
    }

    /// @brief 在当前文件夹下创建文件。
    ///
    /// @param name 文件名
    /// @param fs 当前文件夹所属的文件系统
    pub fn create_file(&self, name: &str, fs: &Arc<FATFileSystem>) -> Result<FATFile, SystemError> {
        let r: Result<FATDirEntryOrShortName, SystemError> =
            self.check_existence(name, Some(false), fs.clone());
        // 检查错误码，如果能够表明目录项已经存在，则返回-EEXIST
        if let Err(err_val) = r {
            if err_val == (SystemError::EISDIR) || err_val == (SystemError::ENOTDIR) {
                return Err(SystemError::EEXIST);
            } else {
                return Err(err_val);
            }
        }

        match r.unwrap() {
            FATDirEntryOrShortName::ShortName(short_name) => {
                // 确认名称是一个可行的长文件名
                LongDirEntry::validate_long_name(name)?;
                // 创建目录项
                let x: Result<FATFile, SystemError> = self
                    .create_dir_entries(
                        name.trim(),
                        &short_name,
                        None,
                        FileAttributes {
                            value: FileAttributes::ARCHIVE,
                        },
                        fs.clone(),
                    )
                    .map(|e| e.to_file())?;
                return x;
            }

            FATDirEntryOrShortName::DirEntry(_) => {
                // 已经存在这样的一个目录项了
                return Err(SystemError::EEXIST);
            }
        }
    }

    pub fn create_dir(&self, name: &str, fs: &Arc<FATFileSystem>) -> Result<FATDir, SystemError> {
        let r: Result<FATDirEntryOrShortName, SystemError> =
            self.check_existence(name, Some(true), fs.clone());
        // debug!("check existence ok");
        // 检查错误码，如果能够表明目录项已经存在，则返回-EEXIST
        if let Err(err_val) = r {
            if err_val == (SystemError::EISDIR) || err_val == (SystemError::ENOTDIR) {
                return Err(SystemError::EEXIST);
            } else {
                return Err(err_val);
            }
        }

        match r.unwrap() {
            // 文件夹不存在，创建文件夹
            FATDirEntryOrShortName::ShortName(short_name) => {
                LongDirEntry::validate_long_name(name)?;
                // 目标目录项
                let mut short_entry = ShortDirEntry::default();

                let first_cluster: Cluster = fs.allocate_cluster(None)?;
                short_entry.set_first_cluster(first_cluster);

                // === 接下来在子目录中创建'.'目录项和'..'目录项
                let mut offset = 0;
                // '.'目录项
                let mut dot_entry = ShortDirEntry {
                    name: ShortNameGenerator::new(".").generate().unwrap(),
                    attributes: FileAttributes::new(FileAttributes::DIRECTORY),
                    ..Default::default()
                };
                dot_entry.set_first_cluster(first_cluster);

                // todo: 设置创建、访问时间
                dot_entry.flush(fs, fs.cluster_bytes_offset(first_cluster) + offset)?;

                // 偏移量加上一个目录项的长度
                offset += FATRawDirEntry::DIR_ENTRY_LEN;

                // '..'目录项
                let mut dot_dot_entry = ShortDirEntry {
                    name: ShortNameGenerator::new("..").generate().unwrap(),
                    attributes: FileAttributes::new(FileAttributes::DIRECTORY),
                    ..Default::default()
                };
                dot_dot_entry.set_first_cluster(self.first_cluster);
                // todo: 设置创建、访问时间

                dot_dot_entry.flush(fs, fs.cluster_bytes_offset(first_cluster) + offset)?;

                // debug!("to create dentries");
                // 在当前目录下创建目标目录项
                let res = self
                    .create_dir_entries(
                        name.trim(),
                        &short_name,
                        Some(short_entry),
                        FileAttributes {
                            value: FileAttributes::DIRECTORY,
                        },
                        fs.clone(),
                    )
                    .map(|e| e.to_dir())?;
                // debug!("create dentries ok");
                return res;
            }
            FATDirEntryOrShortName::DirEntry(_) => {
                // 已经存在这样的一个目录项了
                return Err(SystemError::EEXIST);
            }
        }
    }
    /// @brief 检查目录项在当前文件夹下是否存在
    ///
    /// @param name 目录项的名字
    /// @param expect_dir 该值为Some时有效。如果期待目标目录项是文件夹，那么值为Some(true), 否则为Some(false).
    /// @param fs 当前目录所属的文件系统
    ///
    /// @return Ok(FATDirEntryOrShortName::DirEntry) 找到期待的目录项
    /// @return Ok(FATDirEntryOrShortName::ShortName) 当前文件夹下不存在指定的目录项，因此返回一个可行的短文件名
    /// @return Err(SystemError) 错误码
    pub fn check_existence(
        &self,
        name: &str,
        expect_dir: Option<bool>,
        fs: Arc<FATFileSystem>,
    ) -> Result<FATDirEntryOrShortName, SystemError> {
        let mut sng = ShortNameGenerator::new(name);

        loop {
            let e: Result<FATDirEntry, SystemError> =
                self.find_entry(name, expect_dir, Some(&mut sng), fs.clone());
            match e {
                Ok(e) => {
                    // 找到，返回目录项
                    return Ok(FATDirEntryOrShortName::DirEntry(e));
                }
                Err(e) => {
                    // 如果没找到，则不返回错误
                    if e == SystemError::ENOENT {
                    } else {
                        // 其他错误，则返回
                        return Err(e);
                    }
                }
            }

            // 没找到文件，则生成短文件名
            if let Ok(name) = sng.generate() {
                return Ok(FATDirEntryOrShortName::ShortName(name));
            }

            sng.next_iteration();
        }
    }

    /// @brief 创建一系列的目录项
    ///
    /// @param long_name 长文件名
    /// @param short_name 短文件名
    /// @param short_dentry 可选的生成好的短目录项结构体
    /// @param attrs FAT目录项的属性
    /// @param fs 当前文件夹所属的文件系统
    ///
    /// @return Ok(FATDirEntry) FAT目录项的枚举类型（目录项链条的最后一个长目录项）
    fn create_dir_entries(
        &self,
        long_name: &str,
        short_name: &[u8; 11],
        short_dentry: Option<ShortDirEntry>,
        attrs: FileAttributes,
        fs: Arc<FATFileSystem>,
    ) -> Result<FATDirEntry, SystemError> {
        let mut short_dentry: ShortDirEntry = short_dentry.unwrap_or_default();
        short_dentry.name = *short_name;
        short_dentry.attributes = attrs;

        // todo: 设置创建时间、修改时间

        let mut long_name_gen: LongNameEntryGenerator =
            LongNameEntryGenerator::new(long_name, short_dentry.checksum());
        let num_entries = long_name_gen.num_entries() as u64;

        // debug!("to find free entries");
        let free_entries: Option<(Cluster, u64)> =
            self.find_free_entries(num_entries, fs.clone())?;
        // 目录项开始位置
        let start_loc: (Cluster, u64) = match free_entries {
            Some(c) => c,
            None => return Err(SystemError::ENOSPC),
        };
        let offsets: Vec<(Cluster, u64)> =
            FATDirEntryOffsetIter::new(fs.clone(), start_loc, num_entries, None).collect();

        // 迭代长目录项
        for off in &offsets.as_slice()[..offsets.len() - 1] {
            // 获取生成的下一个长目录项
            let long_entry: LongDirEntry = long_name_gen.next().unwrap();
            // 获取这个长目录项在分区内的字节偏移量
            let bytes_offset = fs.cluster_bytes_offset(off.0) + off.1;
            long_entry.flush(fs.clone(), bytes_offset)?;
        }

        let start: (Cluster, u64) = offsets[0];
        let end: (Cluster, u64) = *offsets.last().unwrap();
        // 短目录项在分区内的字节偏移量
        let offset = fs.cluster_bytes_offset(end.0) + end.1;
        short_dentry.flush(&fs, offset)?;

        return Ok(
            short_dentry.convert_to_dir_entry_with_long_name(long_name.to_string(), (start, end))
        );
    }

    /// @brief 判断当前目录是否为空
    ///
    /// @return true 当前目录为空
    /// @return false 当前目录不为空
    pub fn is_empty(&self, fs: Arc<FATFileSystem>) -> bool {
        for e in self.to_iter(fs) {
            let s = e.short_name();
            if s == "." || s == ".." {
                continue;
            } else {
                return false;
            }
        }
        return true;
    }

    /// @brief 从当前文件夹中删除文件或者文件夹。如果目标文件夹不为空，则不能删除，返回-ENOTEMPTY.
    ///
    /// @param fs 当前FATDir所属的文件系统
    /// @param name 目录项的名字
    /// @param remove_clusters 是否删除与指定的目录项相关联的数据簇
    ///
    /// @return Ok() 成功时无返回值
    /// @return Err(SystemError) 如果目标文件夹不为空，则不能删除，返回-ENOTEMPTY. 或者返回底层传上来的错误
    pub fn remove(
        &self,
        fs: Arc<FATFileSystem>,
        name: &str,
        remove_clusters: bool,
    ) -> Result<(), SystemError> {
        let e: FATDirEntry = self.find_entry(name, None, None, fs.clone())?;

        // 判断文件夹是否为空，如果空，则不删除，报错。
        if e.is_dir() && !(e.to_dir().unwrap().is_empty(fs.clone())) {
            return Err(SystemError::ENOTEMPTY);
        }

        if e.first_cluster().cluster_num >= 2 && remove_clusters {
            // 删除与指定的目录项相关联的数据簇
            fs.deallocate_cluster_chain(e.first_cluster())?;
        }

        if e.get_dir_range().is_some() {
            self.remove_dir_entries(fs, e.get_dir_range().unwrap())?;
        }

        return Ok(());
    }

    /// @brief 在当前目录中删除多个目录项
    ///
    /// @param fs 当前目录所属的文件系统
    /// @param cluster_range 要删除的目录项的范围（以簇+簇内偏移量的形式表示）
    fn remove_dir_entries(
        &self,
        fs: Arc<FATFileSystem>,
        cluster_range: ((Cluster, u64), (Cluster, u64)),
    ) -> Result<(), SystemError> {
        // 收集所有的要移除的目录项
        let offsets: Vec<(Cluster, u64)> =
            FATDirEntryOffsetIter::new(fs.clone(), cluster_range.0, 15, Some(cluster_range.1))
                .collect();
        // 逐个设置这些目录项为“空闲”状态
        for off in offsets {
            let gendisk_bytes_offset = fs.cluster_bytes_offset(off.0) + off.1;
            let mut short_entry = ShortDirEntry::default();
            short_entry.name[0] = 0xe5;
            short_entry.flush(&fs, gendisk_bytes_offset)?;
        }
        return Ok(());
    }

    /// @brief 根据名字在当前文件夹下寻找目录项
    ///
    /// @return Ok(FATDirEntry) 目标目录项
    /// @return Err(SystemError) 底层传上来的错误码
    pub fn get_dir_entry(
        &self,
        fs: Arc<FATFileSystem>,
        name: &str,
    ) -> Result<FATDirEntry, SystemError> {
        if name == "." || name == "/" {
            return Ok(FATDirEntry::Dir(self.clone()));
        }

        LongDirEntry::validate_long_name(name)?;
        return self.find_entry(name, None, None, fs);
    }

    /// @brief 在当前目录内，重命名一个目录项
    ///
    pub fn rename(
        &self,
        fs: Arc<FATFileSystem>,
        old_name: &str,
        new_name: &str,
    ) -> Result<FATDirEntry, SystemError> {
        // 判断源目录项是否存在
        let old_dentry: FATDirEntry = if let FATDirEntryOrShortName::DirEntry(dentry) =
            self.check_existence(old_name, None, fs.clone())?
        {
            dentry
        } else {
            // 如果目标目录项不存在，则返回错误
            return Err(SystemError::ENOENT);
        };

        let short_name = if let FATDirEntryOrShortName::ShortName(s) =
            self.check_existence(new_name, None, fs.clone())?
        {
            s
        } else {
            // 如果目标目录项存在，那么就返回错误
            return Err(SystemError::EEXIST);
        };

        let old_short_dentry: Option<ShortDirEntry> = old_dentry.short_dir_entry();
        if let Some(se) = old_short_dentry {
            // 删除原来的目录项
            self.remove(fs.clone(), old_dentry.name().as_str(), false)?;

            // 创建新的目录项
            let new_dentry: FATDirEntry = self.create_dir_entries(
                new_name,
                &short_name,
                Some(se),
                se.attributes,
                fs.clone(),
            )?;

            return Ok(new_dentry);
        } else {
            // 不允许对根目录项进行重命名
            return Err(SystemError::EPERM);
        }
    }

    /// @brief 跨目录，重命名一个目录项
    ///
    pub fn rename_across(
        &self,
        fs: Arc<FATFileSystem>,
        target: &FATDir,
        old_name: &str,
        new_name: &str,
    ) -> Result<FATDirEntry, SystemError> {
        // 判断源目录项是否存在
        let old_dentry: FATDirEntry = if let FATDirEntryOrShortName::DirEntry(dentry) =
            self.check_existence(old_name, None, fs.clone())?
        {
            dentry
        } else {
            // 如果目标目录项不存在，则返回错误
            return Err(SystemError::ENOENT);
        };

        let short_name = if let FATDirEntryOrShortName::ShortName(s) =
            target.check_existence(new_name, None, fs.clone())?
        {
            s
        } else {
            // 如果目标目录项存在，那么就返回错误
            return Err(SystemError::EEXIST);
        };

        let old_short_dentry: Option<ShortDirEntry> = old_dentry.short_dir_entry();
        if let Some(se) = old_short_dentry {
            // 删除原来的目录项
            self.remove(fs.clone(), old_dentry.name().as_str(), false)?;

            // 创建新的目录项
            let new_dentry: FATDirEntry = target.create_dir_entries(
                new_name,
                &short_name,
                Some(se),
                se.attributes,
                fs.clone(),
            )?;

            return Ok(new_dentry);
        } else {
            // 不允许对根目录项进行重命名
            return Err(SystemError::EPERM);
        }
    }
}

impl FileAttributes {
    pub const READ_ONLY: u8 = 1 << 0;
    pub const HIDDEN: u8 = 1 << 1;
    pub const SYSTEM: u8 = 1 << 2;
    pub const VOLUME_ID: u8 = 1 << 3;
    pub const DIRECTORY: u8 = 1 << 4;
    pub const ARCHIVE: u8 = 1 << 5;
    pub const LONG_NAME: u8 = FileAttributes::READ_ONLY
        | FileAttributes::HIDDEN
        | FileAttributes::SYSTEM
        | FileAttributes::VOLUME_ID;

    /// @brief 判断属性是否存在
    #[inline]
    pub fn contains(&self, attr: u8) -> bool {
        return (self.value & attr) != 0;
    }

    pub fn new(attr: u8) -> Self {
        return Self { value: attr };
    }
}

/// FAT32的短目录项
#[derive(Debug, Clone, Copy, Default)]
pub struct ShortDirEntry {
    /// short name
    name: [u8; 11],
    /// 目录项属性 (见 FileAttributes )
    attributes: FileAttributes,

    /// Windows NT系统的保留字段。用来表示短目录项文件名。
    /// EXT|BASE => 8(BASE).3(EXT)
    /// BASE:LowerCase(8),UpperCase(0)
    /// EXT:LowerCase(16),UpperCase(0)
    nt_res: u8,

    /// 文件创建时间的毫秒级时间戳
    crt_time_tenth: u8,
    /// 创建时间
    crt_time: u16,
    /// 创建日期
    crt_date: u16,
    /// 最后一次访问日期
    lst_acc_date: u16,
    /// High word of first cluster(0 for FAT12 and FAT16)
    fst_clus_hi: u16,
    /// 最后写入时间
    wrt_time: u16,
    /// 最后写入日期
    wrt_date: u16,
    /// Low word of first cluster
    fst_clus_lo: u16,
    /// 文件大小
    file_size: u32,
}

/// FAT32的长目录项
#[derive(Debug, Clone, Copy, Default)]
pub struct LongDirEntry {
    /// 长目录项的序号
    ord: u8,
    /// 长文件名的第1-5个字符，每个字符占2bytes
    name1: [u16; 5],
    /// 目录项属性必须为ATTR_LONG_NAME
    file_attrs: FileAttributes,
    /// Entry Type: 如果为0，则说明这是长目录项的子项
    /// 非零值是保留的。
    dirent_type: u8,
    /// 短文件名的校验和
    checksum: u8,
    /// 长文件名的第6-11个字符，每个字符占2bytes
    name2: [u16; 6],
    /// 必须为0
    first_clus_low: u16,
    /// 长文件名的12-13个字符，每个字符占2bytes
    name3: [u16; 2],
}

impl LongDirEntry {
    /// 长目录项的字符串长度（单位：word）
    pub const LONG_NAME_STR_LEN: usize = 13;

    /// @brief 初始化一个新的长目录项
    ///
    /// @param ord 顺序
    /// @param name_part 长目录项名称的数组（长度必须为13）
    /// @param check_sum 短目录项的校验和
    ///
    /// @return Self 初始化好的长目录项对象
    fn new(ord: u8, name_part: &[u16], check_sum: u8) -> Self {
        let mut result = LongDirEntry {
            ord,
            file_attrs: FileAttributes::new(FileAttributes::LONG_NAME),
            dirent_type: 0,
            checksum: check_sum,
            ..Default::default()
        };
        result
            .insert_name(name_part)
            .expect("Name part's len should be equal to 13.");
        // 该字段需要外层的代码手动赋值
        result.first_clus_low = 0;
        return result;
    }

    /// @brief 填写长目录项的名称字段。
    ///
    /// @param name_part 要被填入当前长目录项的名字（数组长度必须为13）
    ///
    /// @return Ok(())
    /// @return Err(SystemError) 错误码
    fn insert_name(&mut self, name_part: &[u16]) -> Result<(), SystemError> {
        if name_part.len() != Self::LONG_NAME_STR_LEN {
            return Err(SystemError::EINVAL);
        }
        self.name1.copy_from_slice(&name_part[0..5]);
        self.name2.copy_from_slice(&name_part[5..11]);
        self.name3.copy_from_slice(&name_part[11..13]);
        return Ok(());
    }

    /// @brief 将当前长目录项的名称字段，原样地拷贝到一个长度为13的u16数组中。
    /// @param dst 拷贝的目的地，一个[u16]数组，长度必须为13。
    pub fn copy_name_to_slice(&self, dst: &mut [u16]) -> Result<(), SystemError> {
        if dst.len() != Self::LONG_NAME_STR_LEN {
            return Err(SystemError::EINVAL);
        }
        dst[0..5].copy_from_slice(&self.name1);
        dst[5..11].copy_from_slice(&self.name2);
        dst[11..13].copy_from_slice(&self.name3);
        return Ok(());
    }

    /// @brief 是否为最后一个长目录项
    ///
    /// @return true 是最后一个长目录项
    /// @return false 不是最后一个长目录项
    pub fn is_last(&self) -> bool {
        return self.ord & 0x40 > 0;
    }

    /// @brief 校验字符串是否符合长目录项的命名要求
    ///
    /// @return Ok(()) 名称合法
    /// @return Err(SystemError) 名称不合法，返回错误码
    pub fn validate_long_name(mut name: &str) -> Result<(), SystemError> {
        // 去除首尾多余的空格
        name = name.trim();

        // 名称不能为0
        if name.is_empty() {
            return Err(SystemError::EINVAL);
        }

        // 名称长度不能大于255
        if name.len() > 255 {
            return Err(SystemError::ENAMETOOLONG);
        }

        // 检查是否符合命名要求
        for c in name.chars() {
            match c {
                'a'..='z' | 'A'..='Z' | '0'..='9' => {}
                '\u{80}'..='\u{ffff}' => {}
                '$' | '%' | '\'' | '-' | '_' | '@' | '~' | '`' | '!' | '(' | ')' | '{' | '}'
                | '^' | '#' | '&' => {}
                '+' | ',' | ';' | '=' | '[' | ']' | '.' | ' ' => {}
                _ => {
                    debug!("error char: {}", c);
                    return Err(SystemError::EILSEQ);
                }
            }
        }
        return Ok(());
    }

    /// @brief 把当前长目录项写入磁盘
    ///
    /// @param fs 对应的文件系统
    /// @param disk_bytes_offset 长目录项所在位置对应的在分区内的字节偏移量
    ///
    /// @return Ok(())
    /// @return Err(SystemError) 错误码
    pub fn flush(
        &self,
        fs: Arc<FATFileSystem>,
        gendisk_bytes_offset: u64,
    ) -> Result<(), SystemError> {
        // 从磁盘读取数据
        let blk_offset = fs.get_in_block_offset(gendisk_bytes_offset);
        let lba = fs.gendisk_lba_from_offset(fs.bytes_to_sector(gendisk_bytes_offset));
        let mut v: Vec<u8> = vec![0; fs.lba_per_sector() * LBA_SIZE];
        fs.gendisk.read_at(&mut v, lba)?;

        let mut cursor: VecCursor = VecCursor::new(v);
        // 切换游标到对应位置
        cursor.seek(SeekFrom::SeekSet(blk_offset as i64))?;

        // 写入数据
        cursor.write_u8(self.ord)?;
        for b in &self.name1 {
            cursor.write_u16(*b)?;
        }

        cursor.write_u8(self.file_attrs.value)?;
        cursor.write_u8(self.dirent_type)?;
        cursor.write_u8(self.checksum)?;

        for b in &self.name2 {
            cursor.write_u16(*b)?;
        }

        cursor.write_u16(self.first_clus_low)?;

        for b in &self.name3 {
            cursor.write_u16(*b)?;
        }

        // 把修改后的长目录项刷入磁盘
        fs.gendisk.write_at(cursor.as_slice(), lba)?;

        fs.gendisk.sync()?;

        return Ok(());
    }
}

impl ShortDirEntry {
    const PADDING: u8 = b' ';

    /// @brief 判断当前目录项是否为文件夹
    ///
    /// @return true 是文件夹
    /// @return false 不是文件夹
    pub fn is_dir(&self) -> bool {
        return (self.attributes.contains(FileAttributes::DIRECTORY))
            && (!self.attributes.contains(FileAttributes::VOLUME_ID));
    }

    /// @brief 判断当前目录项是否为文件
    ///
    /// @return true 是文件
    /// @return false 不是文件
    pub fn is_file(&self) -> bool {
        return (!self.attributes.contains(FileAttributes::DIRECTORY))
            && (!self.attributes.contains(FileAttributes::VOLUME_ID));
    }

    /// @brief 判断当前目录项是否为卷号
    ///
    /// @return true 是卷号
    /// @return false 不是卷号
    pub fn is_volume_id(&self) -> bool {
        return (!self.attributes.contains(FileAttributes::DIRECTORY))
            && self.attributes.contains(FileAttributes::VOLUME_ID);
    }

    /// @brief 将短目录项的名字转换为String
    fn name_to_string(&self) -> String {
        // 计算基础名的长度
        let base_len = self.name[..8]
            .iter()
            .rposition(|x| *x != ShortDirEntry::PADDING)
            .map(|len| len + 1)
            .unwrap_or(0);
        // 计算扩展名的长度
        let ext_len = self.name[8..]
            .iter()
            .rposition(|x| *x != ShortDirEntry::PADDING)
            .map(|len| len + 1)
            .unwrap_or(0);

        // 声明存储完整名字的数组（包含“.”）
        let mut name = [ShortDirEntry::PADDING; 12];
        // 拷贝基础名
        name[..base_len].copy_from_slice(&self.name[..base_len]);

        // 拷贝扩展名，并计算总的长度
        let total_len = if ext_len > 0 {
            name[base_len] = b'.';
            name[base_len + 1..base_len + 1 + ext_len].copy_from_slice(&self.name[8..8 + ext_len]);
            // 总长度为基础名长度+点号+扩展名长度
            base_len + 1 + ext_len
        } else {
            base_len
        };

        if name[0] == 0x05 {
            name[0] = 0xe5;
        }

        let iter = name[..total_len].iter().map(|c| decode_u8_ascii(*c));
        // 返回最终的字符串
        return String::from_iter(iter);
    }

    /// @brief 将短目录项结构体，转换为FATDirEntry枚举类型
    ///
    /// @param loc 当前文件的起始、终止簇。格式：(簇，簇内偏移量)
    /// @return 生成的FATDirENtry枚举类型
    pub fn convert_to_dir_entry(&self, loc: (Cluster, u64)) -> FATDirEntry {
        // 当前文件的第一个簇
        let first_cluster =
            Cluster::new(((self.fst_clus_hi as u64) << 16) | (self.fst_clus_lo as u64));

        // 当前是文件或卷号
        if self.is_file() || self.is_volume_id() {
            let file: FATFile = FATFile {
                file_name: self.name_to_string(),
                first_cluster,
                short_dir_entry: *self,
                loc: (loc, loc),
            };

            // 根据当前短目录项的类型的不同，返回对应的枚举类型。
            if self.is_file() {
                return FATDirEntry::File(file);
            } else {
                return FATDirEntry::VolId(file);
            }
        } else {
            // 当前是文件夹
            let dir = FATDir {
                dir_name: self.name_to_string(),
                first_cluster,
                root_offset: None,
                short_dir_entry: Some(*self),
                loc: Some((loc, loc)),
            };

            return FATDirEntry::Dir(dir);
        }
    }

    /// @brief 将短目录项结构体，转换为FATDirEntry枚举类型. 并且，该短目录项具有对应的长目录项。
    /// 因此，需要传入从长目录项获得的完整的文件名
    ///
    /// @param name 从长目录项获取的完整文件名
    /// @param loc 当前文件的起始、终止簇。格式：(簇，簇内偏移量)
    /// @return 生成的FATDirENtry枚举类型
    pub fn convert_to_dir_entry_with_long_name(
        &self,
        name: String,
        loc: ((Cluster, u64), (Cluster, u64)),
    ) -> FATDirEntry {
        // 当前文件的第一个簇
        let first_cluster =
            Cluster::new(((self.fst_clus_hi as u64) << 16) | (self.fst_clus_lo as u64));

        if self.is_file() || self.is_volume_id() {
            let file = FATFile {
                first_cluster,
                file_name: name,
                loc,
                short_dir_entry: *self,
            };

            if self.is_file() {
                return FATDirEntry::File(file);
            } else {
                return FATDirEntry::VolId(file);
            }
        } else {
            let dir = FATDir {
                first_cluster,
                dir_name: name,
                loc: Some(loc),
                short_dir_entry: Some(*self),
                root_offset: None,
            };

            return FATDirEntry::Dir(dir);
        }
    }

    /// @brief 计算短目录项的名称的校验和
    #[allow(clippy::manual_rotate)]
    fn checksum(&self) -> u8 {
        let mut result = 0;

        for c in &self.name {
            result = (result << 7) + (result >> 1) + *c;
        }
        return result;
    }

    /// # 把当前短目录项写入磁盘
    ///
    /// ## 参数
    ///
    /// - fs 对应的文件系统
    /// - gendisk_bytes_offset 短目录项所在位置对应的在分区内的字节偏移量
    ///
    /// # 返回值
    /// - Ok(())
    /// - Err(SystemError) 错误码
    pub fn flush(
        &self,
        fs: &Arc<FATFileSystem>,
        gendisk_bytes_offset: u64,
    ) -> Result<(), SystemError> {
        // 从磁盘读取数据
        let blk_offset = fs.get_in_block_offset(gendisk_bytes_offset);
        let lba = fs.gendisk_lba_from_offset(fs.bytes_to_sector(gendisk_bytes_offset));
        let mut v: Vec<u8> = vec![0; fs.lba_per_sector() * LBA_SIZE];
        fs.gendisk.read_at(&mut v, lba)?;

        let mut cursor: VecCursor = VecCursor::new(v);
        // 切换游标到对应位置
        cursor.seek(SeekFrom::SeekSet(blk_offset as i64))?;
        cursor.write_exact(&self.name)?;
        cursor.write_u8(self.attributes.value)?;
        cursor.write_u8(self.nt_res)?;
        cursor.write_u8(self.crt_time_tenth)?;
        cursor.write_u16(self.crt_time)?;
        cursor.write_u16(self.crt_date)?;
        cursor.write_u16(self.lst_acc_date)?;
        cursor.write_u16(self.fst_clus_hi)?;
        cursor.write_u16(self.wrt_time)?;
        cursor.write_u16(self.wrt_date)?;
        cursor.write_u16(self.fst_clus_lo)?;
        cursor.write_u32(self.file_size)?;

        // 把修改后的长目录项刷入磁盘
        fs.gendisk.write_at(cursor.as_slice(), lba)?;

        fs.gendisk.sync()?;

        return Ok(());
    }

    /// @brief 设置短目录项的“第一个簇”字段的值
    pub fn set_first_cluster(&mut self, cluster: Cluster) {
        self.fst_clus_lo = (cluster.cluster_num & 0x0000ffff) as u16;
        self.fst_clus_hi = ((cluster.cluster_num & 0xffff0000) >> 16) as u16;
    }
}

/// @brief FAT文件系统标准定义的目录项
#[derive(Debug, Clone)]
pub enum FATRawDirEntry {
    /// 短目录项
    Short(ShortDirEntry),
    /// 长目录项
    Long(LongDirEntry),
    /// 当前目录项的Name[0]==0xe5, 是空闲目录项
    Free,
    /// 当前目录项的Name[0]==0xe5, 是空闲目录项，且在这之后没有被分配过的目录项了。
    FreeRest,
}

impl FATRawDirEntry {
    /// 每个目录项的长度（单位：字节）
    pub const DIR_ENTRY_LEN: u64 = 32;

    /// @brief 判断当前目录项是否为这个文件的最后一个目录项
    fn is_last(&self) -> bool {
        match *self {
            Self::Short(_) => {
                return true;
            }
            Self::Long(l) => {
                return l.is_last();
            }
            _ => {
                return false;
            }
        }
    }

    /// @brief 判断当前目录项是否为长目录项
    fn is_long(&self) -> bool {
        if let Self::Long(_) = self {
            return true;
        } else {
            return false;
        }
    }

    /// @brief 判断当前目录项是否为短目录项
    fn is_short(&self) -> bool {
        if let Self::Short(_) = self {
            return true;
        } else {
            return false;
        }
    }
}

/// @brief FAT文件系统的目录项迭代器
#[derive(Debug)]
pub struct FATDirIter {
    /// 当前正在迭代的簇
    current_cluster: Cluster,
    /// 当前正在迭代的簇的簇内偏移量
    offset: u64,
    /// True for the root directories of FAT12 and FAT16
    is_root: bool,
    /// 指向当前文件系统的指针
    fs: Arc<FATFileSystem>,
}

impl FATDirIter {
    /// @brief 迭代当前inode的目录项(获取下一个目录项)
    ///
    /// @return Ok(Cluster, u64, Option<FATDirEntry>)
    ///             Cluster: 下一个要读取的簇号
    ///             u64: 下一个要读取的簇内偏移量
    ///             Option<FATDirEntry>: 读取到的目录项（如果没有读取到，就返回失败）
    /// @return Err(错误码) 可能出现了内部错误，或者是磁盘错误等。具体原因看错误码。
    fn get_dir_entry(&mut self) -> Result<(Cluster, u64, Option<FATDirEntry>), SystemError> {
        loop {
            if unlikely(self.current_cluster.cluster_num < 2) {
                return Ok((self.current_cluster, self.offset, None));
            }

            // 如果当前簇已经被读完，那么尝试获取下一个簇
            if self.offset >= self.fs.bytes_per_cluster() && !self.is_root {
                match self.fs.get_fat_entry(self.current_cluster)? {
                    FATEntry::Next(c) => {
                        // 获得下一个簇的信息
                        self.current_cluster = c;
                        self.offset %= self.fs.bytes_per_cluster();
                    }

                    _ => {
                        // 没有下一个簇了，返回None
                        return Ok((self.current_cluster, self.offset, None));
                    }
                }
            }

            // 如果当前是FAT12/FAT16文件系统，并且当前inode是根目录项。
            // 如果offset大于根目录项的最大大小（已经遍历完根目录），那么就返回None
            if self.is_root && self.offset > self.fs.root_dir_end_bytes_offset().unwrap() {
                return Ok((self.current_cluster, self.offset, None));
            }

            // 获取簇在分区内的字节偏移量
            let offset: u64 = self.fs.cluster_bytes_offset(self.current_cluster) + self.offset;

            // 从磁盘读取原始的dentry
            let raw_dentry: FATRawDirEntry = get_raw_dir_entry(&self.fs, offset)?;

            // 由于迭代顺序从前往后，因此：
            // 如果找到1个短目录项，那么证明有一个完整的entry被找到，因此返回。
            // 如果找到1个长目录项，那么，就依次往下迭代查找，直到找到一个短目录项，然后返回结果。这里找到的所有的目录项，都属于同一个文件/文件夹。
            match raw_dentry {
                FATRawDirEntry::Short(s) => {
                    // 当前找到一个短目录项，更新offset之后，直接返回
                    self.offset += FATRawDirEntry::DIR_ENTRY_LEN;
                    return Ok((
                        self.current_cluster,
                        self.offset,
                        Some(s.convert_to_dir_entry((
                            self.current_cluster,
                            self.offset - FATRawDirEntry::DIR_ENTRY_LEN,
                        ))),
                    ));
                }
                FATRawDirEntry::Long(_) => {
                    // 当前找到一个长目录项

                    // 声明一个数组，来容纳所有的entry。（先把最后一个entry放进去）
                    let mut long_name_entries: Vec<FATRawDirEntry> = vec![raw_dentry];
                    let start_offset: u64 = self.offset;
                    let start_cluster: Cluster = self.current_cluster;

                    self.offset += FATRawDirEntry::DIR_ENTRY_LEN;

                    // 由于在FAT文件系统中，文件名最长为255字节，因此，最多有20个长目录项以及1个短目录项。
                    // 由于上面已经塞了1个长目录项，因此接下来最多需要迭代20次
                    // 循环查找目录项，直到遇到1个短目录项，或者是空闲目录项
                    for _ in 0..20 {
                        // 如果当前簇已经被读完，那么尝试获取下一个簇
                        if self.offset >= self.fs.bytes_per_cluster() && !self.is_root {
                            match self.fs.get_fat_entry(self.current_cluster)? {
                                FATEntry::Next(c) => {
                                    // 获得下一个簇的信息
                                    self.current_cluster = c;
                                    self.offset %= self.fs.bytes_per_cluster();
                                }

                                _ => {
                                    // 没有下一个簇了，退出迭代
                                    break;
                                }
                            }
                        }
                        // 如果当前是FAT12/FAT16文件系统，并且当前inode是根目录项。
                        // 如果offset大于根目录项的最大大小（已经遍历完根目录），那么就退出迭代
                        if self.is_root
                            && self.offset > self.fs.root_dir_end_bytes_offset().unwrap()
                        {
                            break;
                        }

                        // 获取簇在分区内的字节偏移量
                        let offset: u64 =
                            self.fs.cluster_bytes_offset(self.current_cluster) + self.offset;
                        // 从磁盘读取原始的dentry
                        let raw_dentry: FATRawDirEntry = get_raw_dir_entry(&self.fs, offset)?;

                        match raw_dentry {
                            FATRawDirEntry::Short(_) => {
                                // 当前遇到1个短目录项，证明当前文件/文件夹的所有dentry都被读取完了，因此在将其加入数组后，退出迭代。
                                long_name_entries.push(raw_dentry);
                                break;
                            }
                            FATRawDirEntry::Long(_) => {
                                // 当前遇到1个长目录项，将其加入数组，然后更新offset，继续迭代。
                                long_name_entries.push(raw_dentry);
                                self.offset += FATRawDirEntry::DIR_ENTRY_LEN;
                            }

                            _ => {
                                // 遇到了空闲簇，但没遇到短目录项，说明文件系统出错了，退出。
                                break;
                            }
                        }
                    }
                    // debug!("collect dentries done. long_name_entries={long_name_entries:?}");
                    let dir_entry: Result<FATDirEntry, SystemError> = FATDirEntry::new(
                        long_name_entries,
                        (
                            (start_cluster, start_offset),
                            (self.current_cluster, self.offset),
                        ),
                    );
                    // debug!("dir_entry={:?}", dir_entry);
                    match dir_entry {
                        Ok(d) => {
                            // debug!("dir_entry ok");
                            self.offset += FATRawDirEntry::DIR_ENTRY_LEN;
                            return Ok((self.current_cluster, self.offset, Some(d)));
                        }

                        Err(_) => {
                            // debug!("dir_entry err,  e={}", e);
                            self.offset += FATRawDirEntry::DIR_ENTRY_LEN;
                        }
                    }
                }
                FATRawDirEntry::Free => {
                    // 当前目录项是空的
                    self.offset += FATRawDirEntry::DIR_ENTRY_LEN;
                }
                FATRawDirEntry::FreeRest => {
                    // 当前目录项是空的，且之后都是空的，因此直接返回
                    return Ok((self.current_cluster, self.offset, None));
                }
            }
        }
    }
}

/// 为DirIter实现迭代器trait
impl Iterator for FATDirIter {
    type Item = FATDirEntry;

    fn next(&mut self) -> Option<Self::Item> {
        match self.get_dir_entry() {
            Ok((cluster, offset, result)) => {
                self.current_cluster = cluster;
                self.offset = offset;
                return result;
            }
            Err(_) => {
                return None;
            }
        }
    }
}

impl FATDirEntry {
    /// @brief 构建FATDirEntry枚举类型
    ///
    /// @param long_name_entries 长目录项的数组。
    ///         格式：[第20个（或者是最大ord的那个）, 19, 18, ..., 1, 短目录项]
    ///
    /// @return Ok(FATDirEntry) 构建好的FATDirEntry类型的对象
    /// @return Err(SystemError) 错误码
    pub fn new(
        mut long_name_entries: Vec<FATRawDirEntry>,
        loc: ((Cluster, u64), (Cluster, u64)),
    ) -> Result<Self, SystemError> {
        if long_name_entries.is_empty() {
            return Err(SystemError::EINVAL);
        }

        if !long_name_entries[0].is_last() || !long_name_entries.last().unwrap().is_short() {
            // 存在孤立的目录项，文件系统出现异常，因此返回错误，表明其只读。
            // TODO: 标记整个FAT文件系统为只读的
            return Err(SystemError::EROFS);
        }

        // 取出短目录项（位于vec的末尾）
        let short_dentry: ShortDirEntry = match long_name_entries.pop().unwrap() {
            FATRawDirEntry::Short(s) => s,
            _ => unreachable!(),
        };

        let mut extractor = LongNameExtractor::new();
        for entry in &long_name_entries {
            match entry {
                &FATRawDirEntry::Long(l) => {
                    extractor.process(l)?;
                }

                _ => {
                    return Err(SystemError::EROFS);
                }
            }
        }
        // 检验校验和是否正确
        if extractor.validate_checksum(&short_dentry) {
            // 校验和正确，返回一个长目录项
            return Ok(
                short_dentry.convert_to_dir_entry_with_long_name(extractor.extracted_name(), loc)
            );
        } else {
            // 校验和不相同，认为文件系统出错
            return Err(SystemError::EROFS);
        }
    }

    /// @brief 获取短目录项的名字
    pub fn short_name(&self) -> String {
        match self {
            FATDirEntry::File(f) | FATDirEntry::VolId(f) => {
                return f.short_dir_entry.name_to_string();
            }
            FATDirEntry::Dir(d) => match d.short_dir_entry {
                Some(s) => {
                    return s.name_to_string();
                }
                None => {
                    return String::from("/");
                }
            },
            FATDirEntry::UnInit => unreachable!("FATFS: FATDirEntry uninitialized."),
        }
    }

    /// @brief 获取短目录项结构体
    pub fn short_dir_entry(&self) -> Option<ShortDirEntry> {
        match &self {
            FATDirEntry::File(f) => {
                return Some(f.short_dir_entry);
            }
            FATDirEntry::Dir(d) => {
                return d.short_dir_entry;
            }
            FATDirEntry::VolId(s) => {
                return Some(s.short_dir_entry);
            }
            FATDirEntry::UnInit => unreachable!("FATFS: FATDirEntry uninitialized."),
        }
    }

    /// @brief 获取目录项的第一个簇的簇号
    pub fn first_cluster(&self) -> Cluster {
        match self {
            FATDirEntry::File(f) => {
                return f.first_cluster;
            }
            FATDirEntry::Dir(d) => {
                return d.first_cluster;
            }
            FATDirEntry::VolId(s) => {
                return s.first_cluster;
            }
            FATDirEntry::UnInit => unreachable!("FATFS: FATDirEntry uninitialized."),
        }
    }

    /// @brief 获取当前目录项所占用的簇的范围
    ///
    /// @return (起始簇，簇内偏移量), (终止簇，簇内偏移量)
    pub fn get_dir_range(&self) -> Option<((Cluster, u64), (Cluster, u64))> {
        match self {
            FATDirEntry::File(f) => Some(f.loc),
            FATDirEntry::Dir(d) => d.loc,
            FATDirEntry::VolId(s) => Some(s.loc),
            FATDirEntry::UnInit => unreachable!("FATFS: FATDirEntry uninitialized."),
        }
    }

    /// @brief 获取原始的短目录项名（FAT标准规定的）
    pub fn short_name_raw(&self) -> [u8; 11] {
        match self {
            FATDirEntry::File(f) => {
                return f.short_dir_entry.name;
            }
            FATDirEntry::Dir(d) => match d.short_dir_entry {
                // 存在短目录项，直接返回
                Some(s) => {
                    return s.name;
                }
                // 是根目录项
                None => {
                    let mut s = [0x20u8; 11];
                    s[0] = b'/';
                    return s;
                }
            },
            FATDirEntry::VolId(s) => {
                return s.short_dir_entry.name;
            }

            FATDirEntry::UnInit => unreachable!("FATFS: FATDirEntry uninitialized."),
        }
    }

    /// @brief 获取目录项的名字
    pub fn name(&self) -> String {
        match self {
            FATDirEntry::File(f) => {
                return f.file_name.clone();
            }
            FATDirEntry::VolId(s) => {
                return s.file_name.clone();
            }
            FATDirEntry::Dir(d) => {
                return d.dir_name.clone();
            }
            FATDirEntry::UnInit => unreachable!("FATFS: FATDirEntry uninitialized."),
        }
    }

    /// @brief 判断目录项是否为文件
    pub fn is_file(&self) -> bool {
        matches!(self, &FATDirEntry::File(_) | &FATDirEntry::VolId(_))
    }

    /// @brief 判断目录项是否为文件夹
    pub fn is_dir(&self) -> bool {
        matches!(self, &FATDirEntry::Dir(_))
    }

    /// @brief 判断目录项是否为Volume id
    pub fn is_vol_id(&self) -> bool {
        matches!(self, &FATDirEntry::VolId(_))
    }

    /// @brief 判断FAT目录项的名字与给定的是否相等
    ///
    /// 由于FAT32对大小写不敏感，因此将字符都转为大写，然后比较
    ///
    /// @return bool 相等 => true
    ///              不相等 => false
    pub fn eq_name(&self, name: &str) -> bool {
        // 由于FAT32对大小写不敏感，因此将字符都转为大写，然后比较。
        let binding = self.short_name();
        let short_name = binding.chars().flat_map(|c| c.to_uppercase());
        let binding = self.name();
        let long_name = binding.chars().flat_map(|c| c.to_uppercase());
        let name = name.chars().flat_map(|c| c.to_uppercase());

        let long_name_matches: bool = long_name.eq(name.clone());
        let short_name_matches: bool = short_name.eq(name);

        return long_name_matches || short_name_matches;
    }

    /// @brief 将FATDirEntry转换为FATFile对象
    pub fn to_file(&self) -> Result<FATFile, SystemError> {
        if !self.is_file() {
            return Err(SystemError::EISDIR);
        }

        match &self {
            FATDirEntry::File(f) | FATDirEntry::VolId(f) => {
                return Ok(f.clone());
            }
            _ => unreachable!(),
        }
    }

    /// @brief 将FATDirEntry转换为FATDir对象
    pub fn to_dir(&self) -> Result<FATDir, SystemError> {
        if !self.is_dir() {
            return Err(SystemError::ENOTDIR);
        }
        match &self {
            FATDirEntry::Dir(d) => {
                return Ok(d.clone());
            }
            _ => unreachable!(),
        }
    }
}

/// 用于生成短目录项文件名的生成器。
#[derive(Debug, Default)]
pub struct ShortNameGenerator {
    /// 短目录项的名字
    name: [u8; 11],
    /// 生成器的标志位（使用impl里面的mask来解析）
    flags: u8,
    /// 基础名的长度
    basename_len: u8,
    /// 对于文件名形如(TE021F~1.TXT)的，短前缀+校验码的短目录项，该字段表示基础名末尾数字的对应位。
    checksum_bitmask: u16,
    /// Fletcher-16 Checksum(与填写到ShortDirEntry里面的不一样)
    checksum: u16,
    /// 对于形如(TEXTFI~1.TXT)的短目录项名称，其中的数字的bitmask（第0位置位则表示这个数字是0）
    suffix_bitmask: u16,
}

impl ShortNameGenerator {
    /// 短目录项的名称的长度
    const SHORT_NAME_LEN: usize = 8;

    // ===== flags标志位的含义 =====
    const IS_LOSSY: u8 = (1 << 0);
    const IS_EXACT_MATCH: u8 = (1 << 1);
    const IS_DOT: u8 = (1 << 2);
    const IS_DOTDOT: u8 = (1 << 3);
    /// 名称被完全拷贝
    const NAME_FITS: u8 = (1 << 4);

    /// @brief 初始化一个短目录项名称生成器
    pub fn new(mut name: &str) -> Self {
        name = name.trim();

        let mut short_name: [u8; 11] = [0x20u8; 11];
        if name == "." {
            short_name[0] = b'.';
        }

        if name == ".." {
            short_name[0] = b'.';
            short_name[1] = b'.';
        }

        // @name_fits: 名称是否被完全拷贝
        // @basename_len: 基础名的长度
        // @is_lossy: 是否存在不合法的字符
        let (name_fits, basename_len, is_lossy) = match name.rfind('.') {
            Some(index) => {
                // 文件名里面有".", 且index为最右边的点号所在的下标（bytes index)
                // 拷贝基础名
                let (b_len, fits, b_lossy) =
                    Self::copy_part(&mut short_name[..Self::SHORT_NAME_LEN], &name[..index]);

                // 拷贝扩展名
                let (_, ext_fits, ext_lossy) = Self::copy_part(
                    &mut short_name[Self::SHORT_NAME_LEN..Self::SHORT_NAME_LEN + 3],
                    &name[index + 1..],
                );

                (fits && ext_fits, b_len, b_lossy || ext_lossy)
            }
            None => {
                // 文件名中，不存在"."
                let (b_len, fits, b_lossy) =
                    Self::copy_part(&mut short_name[..Self::SHORT_NAME_LEN], name);
                (fits, b_len, b_lossy)
            }
        };

        let mut flags: u8 = 0;
        // 设置flags
        if is_lossy {
            flags |= Self::IS_LOSSY;
        }
        if name == "." {
            flags |= Self::IS_DOT;
        }
        if name == ".." {
            flags |= Self::IS_DOTDOT;
        }

        if name_fits {
            flags |= Self::NAME_FITS;
        }

        return ShortNameGenerator {
            name: short_name,
            flags,
            basename_len,
            checksum: Self::fletcher_16_checksum(name),
            ..Default::default()
        };
    }

    /// @brief 拷贝字符串到一个u8数组
    ///
    /// @return (u8, bool, bool)
    ///         return.0: 拷贝了的字符串的长度
    ///         return.1: 是否完全拷贝完整个字符串
    ///         return.2: 拷贝过程中，是否出现了不合法字符
    fn copy_part(dest: &mut [u8], src: &str) -> (u8, bool, bool) {
        let mut dest_len: usize = 0;
        let mut lossy_conv = false;

        for c in src.chars() {
            // 如果src还有字符，而dest已经满了，那么表示没有完全拷贝完。
            if dest_len == dest.len() {
                return (dest_len as u8, false, lossy_conv);
            }

            if c == ' ' || c == '.' {
                lossy_conv = true;
                continue;
            }

            let cp: char = match c {
                'a'..='z' | 'A'..='Z' | '0'..='9' => c,
                '$' | '%' | '\'' | '-' | '_' | '@' | '~' | '`' | '!' | '(' | ')' | '{' | '}'
                | '^' | '#' | '&' => c,
                _ => '_',
            };

            // 判断是否存在不符合条件的字符
            lossy_conv = lossy_conv || c != cp;

            // 拷贝字符
            dest[dest_len] = c.to_ascii_uppercase() as u8;
            dest_len += 1;
        }

        // 返回结果
        return (dest_len as u8, true, lossy_conv);
    }

    fn fletcher_16_checksum(name: &str) -> u16 {
        let mut sum1: u16 = 0;
        let mut sum2: u16 = 0;
        for c in name.chars() {
            sum1 = (sum1 + (c as u16)) % 0xff;
            sum2 = (sum1 + sum2) & 0xff;
        }
        return (sum2 << 8) | sum1;
    }

    /// @brief 更新生成器的状态
    /// 当长目录项不存在的时候，需要调用这个函数来更新生成器的状态
    pub fn add_name(&mut self, name: &[u8; 11]) {
        // === 判断名称是否严格的完全匹配
        if name == &self.name {
            self.flags |= Self::IS_EXACT_MATCH;
        }

        // === 检查是否存在长前缀的格式冲突。对于这样的短目录项名称：(TEXTFI~1.TXT)
        // 获取名称前缀
        let prefix_len = min(self.basename_len, 6) as usize;
        // 获取后缀的那个数字
        let num_suffix: Option<u32> = if name[prefix_len] as char == '~' {
            (name[prefix_len + 1] as char).to_digit(10)
        } else {
            None
        };

        // 判断扩展名是否匹配
        let ext_matches: bool = name[8..] == self.name[8..];

        if name[..prefix_len] == self.name[..prefix_len] // 基础名前缀相同
            && num_suffix.is_some() // 基础名具有数字后缀
            && ext_matches
        // 扩展名相匹配
        {
            if let Some(num) = num_suffix {
                self.suffix_bitmask |= 1 << num;
            }
        }

        // === 检查是否存在短前缀+校验和的冲突，文件名形如：(TE021F~1.TXT)
        let prefix_len = min(self.basename_len, 2) as usize;
        let num_suffix: Option<u32> = if name[prefix_len + 4] as char == '~' {
            (name[prefix_len + 1] as char).to_digit(10)
        } else {
            None
        };

        if name[..prefix_len] == self.name[..prefix_len] && num_suffix.is_some() && ext_matches {
            // 获取短文件名中的校验码字段
            let checksum_result: Result<
                Result<u16, core::num::ParseIntError>,
                core::str::Utf8Error,
            > = core::str::from_utf8(&name[prefix_len..prefix_len + 4])
                .map(|s| u16::from_str_radix(s, 16));
            // 如果校验码相同
            if checksum_result == Ok(Ok(self.checksum)) {
                // 置位checksum_bitmask中，基础名末尾数字的对应位
                if let Some(num) = num_suffix {
                    self.checksum_bitmask |= 1 << num;
                }
            }
        }
    }

    pub fn generate(&self) -> Result<[u8; 11], SystemError> {
        if self.is_dot() || self.is_dotdot() {
            return Ok(self.name);
        }

        // 如果当前名字不存在不合法的字符，且名称被完整拷贝，但是exact match为false，可以认为名称没有冲突，直接返回
        if !self.is_lossy() && self.name_fits() && !self.is_exact_match() {
            return Ok(self.name);
        }

        // 尝试使用长前缀（6字符）
        for i in 1..5 {
            if self.suffix_bitmask & (1 << i) == 0 {
                return Ok(self.build_prefixed_name(i as u32, false));
            }
        }

        // 尝试使用短前缀+校验码
        for i in 1..10 {
            if self.checksum_bitmask & (1 << i) == 0 {
                return Ok(self.build_prefixed_name(i as u32, true));
            }
        }
        // 由于产生太多的冲突，因此返回错误（“短文件名已经存在”）
        return Err(SystemError::EEXIST);
    }

    pub fn next_iteration(&mut self) {
        // 在下一次迭代中，尝试一个不同的校验和
        self.checksum = (core::num::Wrapping(self.checksum) + core::num::Wrapping(1)).0;
        // 清空bitmask
        self.suffix_bitmask = 0;
        self.checksum_bitmask = 0;
    }

    /// @brief 构造具有前缀的短目录项名称
    ///
    /// @param num 这是第几个重名的前缀名
    /// @param with_checksum 前缀名中是否包含校验码
    ///
    /// @return 构造好的短目录项名称数组
    fn build_prefixed_name(&self, num: u32, with_checksum: bool) -> [u8; 11] {
        let mut buf: [u8; 11] = [0x20u8; 11];
        let prefix_len: usize = if with_checksum {
            let prefix_len: usize = min(self.basename_len as usize, 2);
            buf[..prefix_len].copy_from_slice(&self.name[..prefix_len]);
            buf[prefix_len..prefix_len + 4].copy_from_slice(&Self::u16_to_u8_array(self.checksum));
            prefix_len + 4
        } else {
            let prefix_len = min(self.basename_len as usize, 6);
            buf[..prefix_len].copy_from_slice(&self.name[..prefix_len]);
            prefix_len
        };

        buf[prefix_len] = b'~';
        buf[prefix_len + 1] = char::from_digit(num, 10).unwrap() as u8;
        buf[8..].copy_from_slice(&self.name[8..]);
        return buf;
    }

    /// @brief 将一个u16数字转换为十六进制大写字符串对应的ascii数组。
    /// 举例：将x=12345转换为16进制字符串“3039”对应的ascii码数组：[51,48,51,57]
    fn u16_to_u8_array(x: u16) -> [u8; 4] {
        let c1 = char::from_digit((x as u32 >> 12) & 0xf, 16)
            .unwrap()
            .to_ascii_uppercase() as u8;
        let c2 = char::from_digit((x as u32 >> 8) & 0xf, 16)
            .unwrap()
            .to_ascii_uppercase() as u8;
        let c3 = char::from_digit((x as u32 >> 4) & 0xf, 16)
            .unwrap()
            .to_ascii_uppercase() as u8;
        let c4 = char::from_digit((x as u32) & 0xf, 16)
            .unwrap()
            .to_ascii_uppercase() as u8;
        return [c1, c2, c3, c4];
    }

    #[inline]
    fn is_lossy(&self) -> bool {
        return (self.flags & Self::IS_LOSSY) > 0;
    }

    #[inline]
    fn is_exact_match(&self) -> bool {
        return (self.flags & Self::IS_EXACT_MATCH) > 0;
    }

    #[inline]
    fn is_dot(&self) -> bool {
        return (self.flags & Self::IS_DOT) > 0;
    }

    #[inline]
    fn is_dotdot(&self) -> bool {
        return (self.flags & Self::IS_DOTDOT) > 0;
    }

    #[inline]
    fn name_fits(&self) -> bool {
        return (self.flags & Self::NAME_FITS) > 0;
    }
}

/// 从多个LongName中提取完整文件名字段的提取器
struct LongNameExtractor {
    name: Vec<u16>,
    checksum: u8,
    index: u8,
}

impl LongNameExtractor {
    fn new() -> Self {
        return LongNameExtractor {
            name: Vec::new(),
            checksum: 0,
            index: 0,
        };
    }

    /// @brief 提取长目录项的名称
    /// @param longname_dentry 长目录项
    /// 请注意，必须倒序输入长目录项对象
    fn process(&mut self, longname_dentry: LongDirEntry) -> Result<(), SystemError> {
        let is_last: bool = longname_dentry.is_last();
        let index: u8 = longname_dentry.ord & 0x1f;

        if index == 0 {
            self.name.clear();
            return Err(SystemError::EROFS);
        }

        // 如果是最后一个LongDirEntry，则初始化当前生成器
        if is_last {
            self.index = index;
            self.checksum = longname_dentry.checksum;
            self.name
                .resize(index as usize * LongDirEntry::LONG_NAME_STR_LEN, 0);
        } else if self.index == 0
            || index != self.index - 1
            || self.checksum != longname_dentry.checksum
        {
            // 如果当前index为0,或者index不连续，或者是校验和不同，那么认为文件系统损坏，清除生成器的名称字段
            // TODO: 对文件系统的变为只读状态状况的拦截
            self.name.clear();
            return Err(SystemError::EROFS);
        } else {
            // 由于dentry倒序输入，因此index是每次减1的
            self.index -= 1;
        }

        let pos: usize = ((index - 1) as usize) * LongDirEntry::LONG_NAME_STR_LEN;
        // 将当前目录项的值，拷贝到生成器的数组中
        longname_dentry
            .copy_name_to_slice(&mut self.name[pos..pos + LongDirEntry::LONG_NAME_STR_LEN])?;
        return Ok(());
    }

    /// 返回名称的长度
    #[inline]
    fn len(&self) -> usize {
        return self.name.len();
    }

    /// 返回抽取得到的名称字符串
    fn extracted_name(&self) -> String {
        let mut s = String::from_utf16_lossy(self.name.as_slice());
        // 计算字符串的长度。如果字符串中有\0，那么就截取字符串的前面部分
        if let Some(len) = s.find('\u{0}') {
            s.truncate(len);
        }
        return s;
    }

    /// @brief 判断校验码是否与指定的短目录项的校验码相同
    ///
    /// @return bool    相同 => true
    ///                 不同 => false
    fn validate_checksum(&self, short_dentry: &ShortDirEntry) -> bool {
        return self.checksum == short_dentry.checksum();
    }
}

/// @brief 长目录项生成器
#[derive(Debug)]
struct LongNameEntryGenerator {
    name: Vec<u16>,
    // 短目录项的校验和
    checksum: u8,
    // 当前迭代器的索引
    idx: u8,
    /// 最后一个目录项的索引
    last_index: u8,
}

impl LongNameEntryGenerator {
    /// @brief 初始化长目录项生成器
    ///
    /// @param name 长文件名数组
    /// @param checksum 短目录项的校验和
    pub fn new(name: &str, checksum: u8) -> Self {
        let mut name: Vec<u16> = name.chars().map(|c| c as u16).collect();

        let padding_bytes: usize = (13 - (name.len() % 13)) % 13;
        // 填充最后一个长目录项的文件名
        for i in 0..padding_bytes {
            if i == 0 {
                name.push(0);
            } else {
                name.push(0xffff);
            }
        }

        // 先从最后一个长目录项开始生成
        let start_index = (name.len() / 13) as u8;
        return LongNameEntryGenerator {
            name,
            checksum,
            idx: start_index,
            last_index: start_index,
        };
    }

    /// @brief 返回要生成的长目录项的总数
    pub fn num_entries(&self) -> u8 {
        return self.last_index + 1;
    }
}

impl Iterator for LongNameEntryGenerator {
    type Item = LongDirEntry;

    fn next(&mut self) -> Option<Self::Item> {
        match self.idx {
            0 => {
                return None;
            }
            // 最后一个长目录项
            n if n == self.last_index => {
                // 最后一个长目录项的ord需要与0x40相或
                let ord: u8 = n | 0x40;
                let start_idx = ((n - 1) * 13) as usize;
                self.idx -= 1;
                return Some(LongDirEntry::new(
                    ord,
                    &self.name.as_slice()[start_idx..start_idx + 13],
                    self.checksum,
                ));
            }
            n => {
                // 其它的长目录项
                let start_idx = ((n - 1) * 13) as usize;
                self.idx -= 1;
                return Some(LongDirEntry::new(
                    n,
                    &self.name.as_slice()[start_idx..start_idx + 13],
                    self.checksum,
                ));
            }
        }
    }
}

#[derive(Debug)]
pub enum FATDirEntryOrShortName {
    DirEntry(FATDirEntry),
    ShortName([u8; 11]),
}

/// @brief 对FAT目录项的迭代器(基于簇和簇内偏移量)
#[derive(Debug)]
struct FATDirEntryOffsetIter {
    /// 当前迭代的偏移量(下一次迭代要返回的值)
    current_offset: (Cluster, u64),
    /// 截止迭代的位置（end_offset所在的位置也会被迭代器返回)
    end_offset: Option<(Cluster, u64)>,
    /// 属于的文件系统
    fs: Arc<FATFileSystem>,
    /// 当前已经迭代了多少次
    index: u64,
    /// 总共要迭代多少次
    len: u64,
    /// 如果end_offset不为None，该字段表示“是否已经到达了迭代终点”
    fin: bool,
}

impl FATDirEntryOffsetIter {
    /// @brief 初始化FAT目录项的迭代器(基于簇和簇内偏移量)
    ///
    /// @param fs 属于的文件系统
    /// @param start 起始偏移量
    /// @param len 要迭代的次数
    /// @param end_offset 截止迭代的位置（end_offset所在的位置也会被迭代器返回)
    ///
    /// @return 构建好的迭代器对象
    pub fn new(
        fs: Arc<FATFileSystem>,
        start: (Cluster, u64),
        len: u64,
        end_offset: Option<(Cluster, u64)>,
    ) -> Self {
        return FATDirEntryOffsetIter {
            current_offset: start,
            end_offset,
            fs,
            index: 0,
            len,
            fin: false,
        };
    }
}

impl Iterator for FATDirEntryOffsetIter {
    type Item = (Cluster, u64);

    fn next(&mut self) -> Option<Self::Item> {
        if self.index == self.len || self.fin {
            return None;
        }

        let r: (Cluster, u64) = self.current_offset;
        // 计算新的字节偏移量
        let mut new_offset = r.1 + FATRawDirEntry::DIR_ENTRY_LEN;
        let mut new_cluster: Cluster = r.0;
        // 越过了当前簇,则获取下一个簇
        if new_offset >= self.fs.bytes_per_cluster() {
            new_offset %= self.fs.bytes_per_cluster();

            match self.fs.get_fat_entry(new_cluster) {
                Ok(FATEntry::Next(c)) => {
                    new_cluster = c;
                }
                // 没有下一个簇了
                _ => {
                    self.fin = true;
                }
            }
        }

        if let Some(off) = self.end_offset {
            // 判断当前簇是否是要求停止搜索的最后一个位置
            self.fin = off == self.current_offset;
        }
        // 更新当前迭代的偏移量
        self.current_offset = (new_cluster, new_offset);
        self.index += 1;

        return Some(r);
    }
}

/// 根据分区字节偏移量，读取磁盘，并生成一个FATRawDirEntry对象
pub fn get_raw_dir_entry(
    fs: &Arc<FATFileSystem>,
    gendisk_bytes_offset: u64,
) -> Result<FATRawDirEntry, SystemError> {
    // 块内偏移量
    let blk_offset: u64 = fs.get_in_block_offset(gendisk_bytes_offset);
    let lba = fs.gendisk_lba_from_offset(fs.bytes_to_sector(gendisk_bytes_offset));

    let mut v: Vec<u8> = vec![0; LBA_SIZE];

    fs.gendisk.read_at(&mut v, lba)?;

    let mut cursor: VecCursor = VecCursor::new(v);
    // 切换游标到对应位置
    cursor.seek(SeekFrom::SeekSet(blk_offset as i64))?;

    let dir_0 = cursor.read_u8()?;

    match dir_0 {
        0x00 => {
            return Ok(FATRawDirEntry::FreeRest);
        }
        0xe5 => {
            return Ok(FATRawDirEntry::Free);
        }
        _ => {
            cursor.seek(SeekFrom::SeekCurrent(10))?;
            let file_attr: FileAttributes = FileAttributes::new(cursor.read_u8()?);

            // 指针回到目录项的开始处
            cursor.seek(SeekFrom::SeekSet(blk_offset as i64))?;

            if file_attr.contains(FileAttributes::LONG_NAME) {
                // 当前目录项是一个长目录项
                let mut long_dentry = LongDirEntry {
                    ord: cursor.read_u8()?,
                    ..Default::default()
                };
                cursor.read_u16_into(&mut long_dentry.name1)?;
                long_dentry.file_attrs = FileAttributes::new(cursor.read_u8()?);
                long_dentry.dirent_type = cursor.read_u8()?;
                long_dentry.checksum = cursor.read_u8()?;

                cursor.read_u16_into(&mut long_dentry.name2)?;
                long_dentry.first_clus_low = cursor.read_u16()?;
                cursor.read_u16_into(&mut long_dentry.name3)?;

                return Ok(FATRawDirEntry::Long(long_dentry));
            } else {
                // 当前目录项是一个短目录项
                let mut short_dentry = ShortDirEntry::default();
                cursor.read_exact(&mut short_dentry.name)?;

                short_dentry.attributes = FileAttributes::new(cursor.read_u8()?);

                short_dentry.nt_res = cursor.read_u8()?;
                short_dentry.crt_time_tenth = cursor.read_u8()?;
                short_dentry.crt_time = cursor.read_u16()?;
                short_dentry.crt_date = cursor.read_u16()?;
                short_dentry.lst_acc_date = cursor.read_u16()?;
                short_dentry.fst_clus_hi = cursor.read_u16()?;
                short_dentry.wrt_time = cursor.read_u16()?;
                short_dentry.wrt_date = cursor.read_u16()?;
                short_dentry.fst_clus_lo = cursor.read_u16()?;
                short_dentry.file_size = cursor.read_u32()?;

                return Ok(FATRawDirEntry::Short(short_dentry));
            }
        }
    }
}
