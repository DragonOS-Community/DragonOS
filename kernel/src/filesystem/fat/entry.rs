#[derive(Debug, Clone, Copy, Default)]
pub struct FileAttributes {
    value: u8,
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
    chksum: u8,
    /// 长文件名的第6-11个字符，每个字符占2bytes
    name2: [u16; 6],
    /// 必须为0
    first_clus_low: u16,
    /// 长文件名的12-13个字符，每个字符占2bytes
    name3: [u16; 2],
}
