use core::cmp::min;

use alloc::{string::String, sync::Arc, vec};

use crate::include::bindings::bindings::{EEXIST, EILSEQ, EINVAL, ENAMETOOLONG, EROFS};

use super::{
    fs::{Cluster, FATFileSystem},
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
#[derive(Debug)]
pub enum FATDirEntry {
    File(FATFile),
    VolId(FATFile),
    Dir(FATDir),
}

/// FAT文件系统中的文件
#[derive(Debug, Default)]
pub struct FATFile {
    /// 文件的第一个簇
    pub first_cluster: Cluster,
    /// 文件名
    pub file_name: String,
    /// 文件对应的短目录项
    pub short_dir_entry: ShortDirEntry,
    /// 文件的起始、终止簇。格式：(簇，簇内偏移量)
    pub loc: ((Cluster, u64), (Cluster, u64)),
}

/// FAT文件系统中的文件夹
#[derive(Debug, Default)]
pub struct FATDir {
    /// 目录的第一个簇
    pub first_cluster: Cluster,
    /// 该字段仅对FAT12、FAT16生效
    pub root_offset: Option<u64>,
    /// 文件夹名称
    pub dir_name: String,
    pub short_dir_entry: Option<ShortDirEntry>,
    /// 文件的起始、终止簇。格式：(簇，簇内偏移量)
    pub loc: Option<((Cluster, u64), (Cluster, u64))>,
}

impl FileAttributes {
    /// @brief 判断属性是否存在
    #[inline]
    pub fn contains(&self, attr: u8) -> bool {
        return (self.value & attr) != 0;
    }
}

impl FileAttributes {
    const READ_ONLY: u8 = 1 << 0;
    const HIDDEN: u8 = 1 << 1;
    const SYSTEM: u8 = 1 << 2;
    const VOLUME_ID: u8 = 1 << 3;
    const DIRECTORY: u8 = 1 << 4;
    const ARCHIVE: u8 = 1 << 5;
    const LONG_NAME: u8 = FileAttributes::READ_ONLY
        | FileAttributes::HIDDEN
        | FileAttributes::SYSTEM
        | FileAttributes::VOLUME_ID;
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
    fst_clst_hi: u16,
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
        let mut result = LongDirEntry::default();
        result.ord = ord;
        result
            .insert_name(name_part)
            .expect("Name part's len should be equal to 13.");
        result.file_attrs.value = FileAttributes::LONG_NAME;
        result.dirent_type = 0;
        result.checksum = check_sum;
        // 该字段需要外层的代码手动赋值
        result.first_clus_low = 0;
        return result;
    }

    /// @brief 填写长目录项的名称字段。
    ///
    /// @param name_part 要被填入当前长目录项的名字（数组长度必须为13）
    ///
    /// @return Ok(())
    /// @return Err(i32) 错误码
    fn insert_name(&mut self, name_part: &[u16]) -> Result<(), i32> {
        if name_part.len() != Self::LONG_NAME_STR_LEN {
            return Err(-(EINVAL as i32));
        }
        self.name1.copy_from_slice(&name_part[0..5]);
        self.name2.copy_from_slice(&name_part[5..11]);
        self.name3.copy_from_slice(&name_part[11..13]);
        return Ok(());
    }

    /// @brief 将当前长目录项的名称字段，原样地拷贝到一个长度为13的u16数组中。
    /// @param dst 拷贝的目的地，一个[u16]数组，长度必须为13。
    pub fn copy_name_to_slice(&self, dst: &mut [u16]) -> Result<(), i32> {
        if dst.len() != Self::LONG_NAME_STR_LEN {
            return Err(-(EINVAL as i32));
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
    /// @return Err(i32) 名称不合法，返回错误码
    pub fn validate_long_name(mut name: &str) -> Result<(), i32> {
        // 去除首尾多余的空格
        name = name.trim();

        // 名称不能为0
        if name.len() == 0 {
            return Err(-(EINVAL as i32));
        }

        // 名称长度不能大于255
        if name.len() > 255 {
            return Err(-(ENAMETOOLONG as i32));
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
                    return Err(-(EILSEQ as i32));
                }
            }
        }
        return Ok(());
    }
}

impl ShortDirEntry {
    const PADDING: u8 = ' ' as u8;

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
            name[base_len] = '.' as u8;
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
    pub fn to_dir_entry(&self, loc: (Cluster, u64)) -> FATDirEntry {
        // 当前文件的第一个簇
        let first_cluster =
            Cluster::new(((self.fst_clst_hi as u64) << 16) | (self.fst_clus_lo as u64));

        // 当前是文件或卷号
        if self.is_file() || self.is_volume_id() {
            let mut file: FATFile = FATFile::default();

            file.file_name = self.name_to_string();
            file.first_cluster = first_cluster;
            file.short_dir_entry = self.clone();
            file.loc = (loc, loc);

            // 根据当前短目录项的类型的不同，返回对应的枚举类型。
            if self.is_file() {
                return FATDirEntry::File(file);
            } else {
                return FATDirEntry::VolId(file);
            }
        } else {
            // 当前是文件夹
            let mut dir = FATDir::default();
            dir.dir_name = self.name_to_string();
            dir.first_cluster = first_cluster;
            dir.root_offset = None;
            dir.short_dir_entry = Some(self.clone());
            dir.loc = Some((loc, loc));

            return FATDirEntry::Dir(dir);
        }
    }

    /// @brief 将短目录项结构体，转换为FATDirEntry枚举类型. 并且，该短目录项具有对应的长目录项。
    /// 因此，需要传入从长目录项获得的完整的文件名
    ///
    /// @param name 从长目录项获取的完整文件名
    /// @param loc 当前文件的起始、终止簇。格式：(簇，簇内偏移量)
    /// @return 生成的FATDirENtry枚举类型
    pub fn to_dir_entry_with_long_name(
        &self,
        name: String,
        loc: ((Cluster, u64), (Cluster, u64)),
    ) -> FATDirEntry {
        // 当前文件的第一个簇
        let first_cluster =
            Cluster::new(((self.fst_clst_hi as u64) << 16) | (self.fst_clus_lo as u64));

        if self.is_file() || self.is_volume_id() {
            let mut file = FATFile::default();

            file.first_cluster = first_cluster;
            file.file_name = name;
            file.loc = loc;
            file.short_dir_entry = self.clone();

            if self.is_file() {
                return FATDirEntry::File(file);
            } else {
                return FATDirEntry::VolId(file);
            }
        } else {
            let mut dir = FATDir::default();

            dir.first_cluster = first_cluster;
            dir.dir_name = name;
            dir.loc = Some(loc);
            dir.short_dir_entry = Some(self.clone());
            dir.root_offset = None;

            return FATDirEntry::Dir(dir);
        }
    }

    /// @brief 计算短目录项的名称的校验和
    fn checksum(&self) -> u8 {
        let mut result = 0;

        for c in &self.name {
            result = (result << 7) + (result >> 1) + *c;
        }
        return result;
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
        match self {
            &Self::Short(_) => {
                return true;
            }
            &Self::Long(l) => {
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
    fn get_dir_entry(&mut self) -> Result<(Cluster, u64, Option<FATDirEntry>), i32> {
        loop {
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

            // 获取簇在磁盘内的字节偏移量
            let offset: u64 = self.fs.cluster_bytes_offset(self.current_cluster) + self.offset;
            // 从磁盘读取原始的dentry
            let raw_dentry: FATRawDirEntry = self.fs.get_raw_dir_entry(offset)?;

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
                        Some(s.to_dir_entry((
                            self.current_cluster,
                            self.offset - FATRawDirEntry::DIR_ENTRY_LEN,
                        ))),
                    ));
                }
                FATRawDirEntry::Long(l) => {
                    // 当前找到一个长目录项

                    // 声明一个数组，来容纳所有的entry。（先把最后一个entry放进去）
                    let mut long_name_entries: vec::Vec<FATRawDirEntry> = vec![raw_dentry];
                    let start_offset: u64 = self.offset;
                    let start_cluster: Cluster = self.current_cluster;

                    self.offset += FATRawDirEntry::DIR_ENTRY_LEN;

                    // 由于在FAT文件系统中，文件名最长为255字节，因此，最多有20个长目录项以及1个短目录项。
                    // 由于上面已经塞了1个长目录项，因此接下来最多需要迭代20次
                    // 循环查找目录项，直到遇到1个短目录项，或者是空闲目录项
                    for i in 0..20 {
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

                        // 获取簇在磁盘内的字节偏移量
                        let offset: u64 =
                            self.fs.cluster_bytes_offset(self.current_cluster) + self.offset;
                        // 从磁盘读取原始的dentry
                        let raw_dentry: FATRawDirEntry = self.fs.get_raw_dir_entry(offset)?;

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
                    let dir_entry: Result<FATDirEntry, i32> = FATDirEntry::new(
                        long_name_entries,
                        (
                            (start_cluster, start_offset),
                            (self.current_cluster, self.offset),
                        ),
                    );

                    match dir_entry {
                        Ok(d) => {
                            self.offset += FATRawDirEntry::DIR_ENTRY_LEN;
                            return Ok((self.current_cluster, self.offset, Some(d)));
                        }

                        Err(_) => {
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
    /// @return Err(i32) 错误码
    pub fn new(
        mut long_name_entries: vec::Vec<FATRawDirEntry>,
        loc: ((Cluster, u64), (Cluster, u64)),
    ) -> Result<Self, i32> {
        if long_name_entries.is_empty() {
            return Err(-(EINVAL as i32));
        }

        if !long_name_entries[0].is_last() || !long_name_entries.last().unwrap().is_short() {
            // 存在孤立的目录项，文件系统出现异常，因此返回错误，表明其只读。
            // TODO: 标记整个FAT文件系统为只读的
            return Err(-(EROFS as i32));
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
                    return Err(-(EROFS as i32));
                }
            }
        }
        // 检验校验和是否正确
        if extractor.validate_checksum(&short_dentry) {
            // 校验和正确，返回一个长目录项
            return Ok(short_dentry.to_dir_entry_with_long_name(extractor.to_string(), loc));
        } else {
            // 校验和不相同，认为文件系统出错
            return Err(-(EROFS as i32));
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
            short_name[0] = '.' as u8;
        }

        if name == ".." {
            short_name[0] = '.' as u8;
            short_name[1] = '.' as u8;
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
                    Self::copy_part(&mut short_name[..Self::SHORT_NAME_LEN], &name);
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
            flags: flags,
            basename_len: basename_len,
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
            let num = num_suffix.unwrap();
            self.suffix_bitmask |= 1 << num;
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
                let num = num_suffix.unwrap();
                // 置位checksum_bitmask中，基础名末尾数字的对应位
                self.checksum_bitmask |= 1 << num;
            }
        }
    }

    pub fn generate(&self) -> Result<[u8; 11], i32> {
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
        return Err(-(EEXIST as i32));
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

        buf[prefix_len] = '~' as u8;
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
        let c4 = char::from_digit((x as u32 >> 0) & 0xf, 16)
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
    name: vec::Vec<u16>,
    checksum: u8,
    index: u8,
}

impl LongNameExtractor {
    fn new() -> Self {
        return LongNameExtractor {
            name: vec::Vec::new(),
            checksum: 0,
            index: 0,
        };
    }

    /// @brief 提取长目录项的名称
    /// @param longname_dentry 长目录项
    /// 请注意，必须倒序输入长目录项对象
    fn process(&mut self, longname_dentry: LongDirEntry) -> Result<(), i32> {
        let is_last: bool = longname_dentry.is_last();
        let index: u8 = longname_dentry.ord & 0x1f;

        if index == 0 {
            self.name.clear();
            return Err(-(EROFS as i32));
        }

        // 如果是最后一个LongDirEntry，则初始化当前生成器
        if is_last {
            self.index = index;
            self.checksum = longname_dentry.checksum;
            self.name
                .resize(index as usize * (FATRawDirEntry::DIR_ENTRY_LEN as usize), 0);
        } else if self.index == 0
            || index != self.index - 1
            || self.checksum != longname_dentry.checksum
        {
            // 如果当前index为0,或者index不连续，或者是校验和不同，那么认为文件系统损坏，清除生成器的名称字段
            // TODO: 对文件系统的变为只读状态状况的拦截
            self.name.clear();
            return Err(-(EROFS as i32));
        } else {
            // 由于dentry倒序输入，因此index是每次减1的
            self.index -= 1;
        }

        let pos: usize = ((index - 1) as usize) * (FATRawDirEntry::DIR_ENTRY_LEN as usize);
        // 将当前目录项的值，拷贝到生成器的数组中
        longname_dentry.copy_name_to_slice(
            &mut self.name[pos..pos + (FATRawDirEntry::DIR_ENTRY_LEN as usize)],
        );
        return Ok(());
    }

    /// @brief 返回名称的长度
    fn len(&self) -> usize {
        return self.name.len();
    }

    /// @brief 返回抽取得到的名称字符串
    fn to_string(&self) -> String {
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
