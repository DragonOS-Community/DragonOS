//! The Super Block is the first field of Ext4 Block Group.
//!
//! See [`super::block_group`] for details.

use super::{crc::crc32, AsBytes};
use crate::constants::CRC32_INIT;
use crate::prelude::*;

// 结构体表示超级块
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SuperBlock {
    inode_count: u32,             // 节点数
    block_count_lo: u32,          // 块数
    reserved_block_count_lo: u32, // 保留块数
    free_block_count_lo: u32,     // 空闲块数
    free_inode_count: u32,        // 空闲节点数
    first_data_block: u32,        // 第一个数据块
    log_block_size: u32,          // Block size is 2 ^ (10 + s_log_block_size).
    log_cluster_size: u32,        // 废弃的片段大小
    blocks_per_group: u32,        // 每组块数
    frags_per_group: u32,         // 废弃的每组片段数
    inodes_per_group: u32,        // 每组节点数
    mount_time: u32,              // 挂载时间
    write_time: u32,              // 写入时间
    mount_count: u16,             // 挂载次数
    max_mount_count: u16,         // 最大挂载次数
    magic: u16,                   // 魔数，0xEF53
    state: u16,                   // 文件系统状态
    errors: u16,                  // 检测到错误时的行为
    minor_rev_level: u16,         // 次版本号
    last_check_time: u32,         // 最后检查时间
    check_interval: u32,          // 检查间隔
    creator_os: u32,              // 创建者操作系统
    rev_level: u32,               // 版本号
    def_resuid: u16,              // 保留块的默认uid
    def_resgid: u16,              // 保留块的默认gid

    // 仅适用于EXT4_DYNAMIC_REV超级块的字段
    first_inode: u32,            // 第一个非保留节点
    inode_size: u16,             // 节点结构的大小
    block_group_index: u16,      // 此超级块的块组索引
    features_compatible: u32,    // 兼容特性集
    features_incompatible: u32,  // 不兼容特性集
    features_read_only: u32,     // 只读兼容特性集
    uuid: [u8; 16],              // 卷的128位uuid
    volume_name: [u8; 16],       // 卷名
    last_mounted: [u8; 64],      // 最后挂载的目录
    algorithm_usage_bitmap: u32, // 用于压缩的算法

    // 性能提示。只有当EXT4_FEATURE_COMPAT_DIR_PREALLOC标志打开时，才进行目录预分配
    s_prealloc_blocks: u8,      // 尝试预分配的块数
    s_prealloc_dir_blocks: u8,  // 为目录预分配的块数
    s_reserved_gdt_blocks: u16, // 在线增长时每组保留的描述符数

    // 如果EXT4_FEATURE_COMPAT_HAS_JOURNAL设置，表示支持日志
    journal_uuid: [u8; 16],    // 日志超级块的UUID
    journal_inode_number: u32, // 日志文件的节点号
    journal_dev: u32,          // 日志文件的设备号
    last_orphan: u32,          // 待删除节点的链表头
    hash_seed: [u32; 4],       // HTREE散列种子
    default_hash_version: u8,  // 默认的散列版本
    journal_backup_type: u8,
    desc_size: u16,            // 组描述符的大小
    default_mount_opts: u32,   // 默认的挂载选项
    first_meta_bg: u32,        // 第一个元数据块组
    mkfs_time: u32,            // 文件系统创建的时间
    journal_blocks: [u32; 17], // 日志节点的备份

    // 如果EXT4_FEATURE_COMPAT_64BIT设置，表示支持64位
    block_count_hi: u32,           // 块数
    reserved_blocks_count_hi: u32, // 保留块数
    free_blocks_count_hi: u32,     // 空闲块数
    min_extra_isize: u16,          // 所有节点至少有#字节
    want_extra_isize: u16,         // 新节点应该保留#字节
    flags: u32,                    // 杂项标志
    raid_stride: u16,              // RAID步长
    mmp_interval: u16,             // MMP检查的等待秒数
    mmp_block: u64,                // 多重挂载保护的块
    raid_stripe_width: u32,        // 所有数据磁盘上的块数（N * 步长）
    log_groups_per_flex: u8,       // FLEX_BG组的大小
    checksum_type: u8,
    reserved_pad: u16,
    kbytes_written: u64,          // 写入的千字节数
    snapshot_inum: u32,           // 活动快照的节点号
    snapshot_id: u32,             // 活动快照的顺序ID
    snapshot_r_blocks_count: u64, // 为活动快照的未来使用保留的块数
    snapshot_list: u32,           // 磁盘上快照列表的头节点号
    error_count: u32,             // 文件系统错误的数目
    first_error_time: u32,        // 第一次发生错误的时间
    first_error_ino: u32,         // 第一次发生错误的节点号
    first_error_block: u64,       // 第一次发生错误的块号
    first_error_func: [u8; 32],   // 第一次发生错误的函数
    first_error_line: u32,        // 第一次发生错误的行号
    last_error_time: u32,         // 最近一次发生错误的时间
    last_error_ino: u32,          // 最近一次发生错误的节点号
    last_error_line: u32,         // 最近一次发生错误的行号
    last_error_block: u64,        // 最近一次发生错误的块号
    last_error_func: [u8; 32],    // 最近一次发生错误的函数
    mount_opts: [u8; 64],
    usr_quota_inum: u32,       // 用于跟踪用户配额的节点
    grp_quota_inum: u32,       // 用于跟踪组配额的节点
    overhead_clusters: u32,    // 文件系统中的开销块/簇
    backup_bgs: [u32; 2],      // 有sparse_super2超级块的组
    encrypt_algos: [u8; 4],    // 使用的加密算法
    encrypt_pw_salt: [u8; 16], // 用于string2key算法的盐
    lpf_ino: u32,              // lost+found节点的位置
    padding: [u32; 100],       // 块的末尾的填充
    checksum: u32,             // crc32c(superblock)
}

unsafe impl AsBytes for SuperBlock {}

impl SuperBlock {
    const SB_MAGIC: u16 = 0xEF53;

    pub const FEATURE_COMPAT_HAS_JOURNAL: u32 = 0x0004;
    pub const FEATURE_COMPAT_FAST_COMMIT: u32 = 0x0400;
    pub const FEATURE_COMPAT_ORPHAN_FILE: u32 = 0x1000;
    pub const FEATURE_INCOMPAT_RECOVER: u32 = 0x0004;
    pub const FEATURE_INCOMPAT_JOURNAL_DEV: u32 = 0x0008;
    /// The orphan file may contain orphaned inodes.
    pub const FEATURE_RO_COMPAT_ORPHAN_PRESENT: u32 = 0x0001_0000;
    pub const FEATURE_RO_COMPAT_METADATA_CSUM: u32 = 0x0400;

    pub fn check_magic(&self) -> bool {
        self.magic == Self::SB_MAGIC
    }

    pub fn first_data_block(&self) -> u32 {
        self.first_data_block
    }

    /// First inode number available for normal (non-reserved) allocation.
    pub fn first_inode(&self) -> u32 {
        self.first_inode
    }

    pub fn free_inodes_count(&self) -> u32 {
        self.free_inode_count
    }

    pub fn uuid(&self) -> [u8; 16] {
        self.uuid
    }

    pub fn compatible_features(&self) -> u32 {
        self.features_compatible
    }

    pub fn incompatible_features(&self) -> u32 {
        self.features_incompatible
    }

    pub fn read_only_compatible_features(&self) -> u32 {
        self.features_read_only
    }

    pub fn has_compatible_feature(&self, feature: u32) -> bool {
        self.features_compatible & feature != 0
    }

    pub fn has_incompatible_feature(&self, feature: u32) -> bool {
        self.features_incompatible & feature != 0
    }

    pub fn has_read_only_compatible_feature(&self, feature: u32) -> bool {
        self.features_read_only & feature != 0
    }

    pub fn set_incompatible_feature(&mut self, feature: u32, enabled: bool) {
        if enabled {
            self.features_incompatible |= feature;
        } else {
            self.features_incompatible &= !feature;
        }
    }

    pub fn journal_inode_number(&self) -> u32 {
        self.journal_inode_number
    }

    pub fn journal_device(&self) -> u32 {
        self.journal_dev
    }

    pub fn journal_uuid(&self) -> [u8; 16] {
        self.journal_uuid
    }

    /// Head inode number of the legacy on-disk orphan list, or zero.
    pub fn last_orphan(&self) -> u32 {
        self.last_orphan
    }

    /// Set the head inode number of the legacy on-disk orphan list.
    ///
    /// The caller must recompute the superblock checksum before persisting it.
    pub fn set_last_orphan(&mut self, inode: u32) {
        self.last_orphan = inode;
    }

    /// Total number of inodes.
    #[allow(unused)]
    pub fn inode_count(&self) -> u32 {
        self.inode_count
    }

    /// Total number of blocks.
    pub fn block_count(&self) -> u64 {
        self.block_count_lo as u64 | ((self.block_count_hi as u64) << 32)
    }

    /// Number of reserved blocks for privileged users.
    pub fn reserved_blocks_count(&self) -> u64 {
        self.reserved_block_count_lo as u64 | ((self.reserved_blocks_count_hi as u64) << 32)
    }

    /// The number of blocks in each block group.
    #[allow(unused)]
    pub fn blocks_per_group(&self) -> u32 {
        self.blocks_per_group
    }

    /// The number of inodes in each block group.
    pub fn inodes_per_group(&self) -> u32 {
        self.inodes_per_group
    }

    /// The number of block groups.
    pub fn block_group_count(&self) -> u32 {
        self.block_count().div_ceil(self.blocks_per_group as u64) as u32
    }

    /// The size of inode.
    pub fn inode_size(&self) -> usize {
        self.inode_size as usize
    }

    /// The size of block group descriptor.
    pub fn desc_size(&self) -> usize {
        self.desc_size as usize
    }

    #[allow(unused)]
    pub fn extra_size(&self) -> u16 {
        self.want_extra_isize
    }

    pub fn inode_count_in_group(&self, bgid: u32) -> u32 {
        let bg_count = self.block_group_count();
        if bgid >= bg_count {
            0
        } else if bgid + 1 < bg_count {
            self.inodes_per_group
        } else {
            self.inode_count
                .saturating_sub((bg_count - 1).saturating_mul(self.inodes_per_group))
        }
    }

    /// Number of allocation clusters represented by a block bitmap.  The
    /// on-disk field historically named fragments-per-group is
    /// s_clusters_per_group in ext4.
    pub fn clusters_per_group(&self) -> u32 {
        self.frags_per_group
    }

    pub fn set_free_inodes_count(&mut self, count: u32) {
        self.free_inode_count = count;
    }

    pub fn free_blocks_count(&self) -> u64 {
        self.free_block_count_lo as u64 | ((self.free_blocks_count_hi as u64) << 32).to_le()
    }

    /// Ext4 metadata overhead stored in superblock, unit: clusters.
    pub fn overhead_clusters(&self) -> u32 {
        self.overhead_clusters
    }

    /// Filesystem block size in bytes.
    pub fn block_size(&self) -> u64 {
        1024u64 << self.log_block_size
    }

    /// Convert cluster count to block count.
    pub fn clusters_to_blocks(&self, clusters: u64) -> u64 {
        if self.log_cluster_size <= self.log_block_size {
            clusters
        } else {
            clusters << (self.log_cluster_size - self.log_block_size)
        }
    }

    pub fn set_free_blocks_count(&mut self, free_blocks: u64) {
        self.free_block_count_lo = ((free_blocks << 32) >> 32).to_le() as u32;
        self.free_blocks_count_hi = (free_blocks >> 32) as u32;
    }

    /// Calc and set the superblock checksum (crc32c).
    ///
    /// Linux (`fs/ext4/super.c` `ext4_superblock_csum`):
    ///   csum = crc32c(~0, superblock_bytes[0 .. offset_of(s_checksum)])
    ///
    /// The UUID is already part of the superblock byte stream so it does NOT
    /// need a separate seed step (unlike inode / block-group checksums).
    pub fn set_checksum(&mut self) {
        let off = core::mem::offset_of!(SuperBlock, checksum);
        let bytes = self.to_bytes();
        self.checksum = crc32(CRC32_INIT, &bytes[..off]);
    }

    pub fn verify_checksum(&self) -> bool {
        let expected = self.checksum;
        let mut copy = *self;
        copy.set_checksum();
        copy.checksum == expected
    }

    pub fn has_supported_checksum_type(&self) -> bool {
        self.checksum_type == 1
    }

    #[cfg(test)]
    pub(crate) fn validation_fixture() -> Self {
        let mut sb: Self = unsafe { core::mem::zeroed() };
        sb.magic = Self::SB_MAGIC;
        sb.block_count_lo = 1024;
        sb.blocks_per_group = 1024;
        sb.frags_per_group = 1024;
        sb.inode_count = 256;
        sb.inodes_per_group = 256;
        sb.first_inode = 11;
        sb.log_block_size = 2;
        sb.log_cluster_size = 2;
        sb.inode_size = 256;
        sb.desc_size = 64;
        sb.features_read_only = Self::FEATURE_RO_COMPAT_METADATA_CSUM;
        sb.checksum_type = 1;
        sb.set_checksum();
        sb
    }

    #[cfg(test)]
    pub(crate) fn set_read_only_compatible_feature(&mut self, feature: u32, enabled: bool) {
        if enabled {
            self.features_read_only |= feature;
        } else {
            self.features_read_only &= !feature;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orphan_fields_and_feature_accessors_roundtrip() {
        // Every bit pattern of SuperBlock's integer/byte fields is valid.
        let mut sb: SuperBlock = unsafe { core::mem::zeroed() };
        sb.first_inode = 11;
        sb.features_read_only = SuperBlock::FEATURE_RO_COMPAT_ORPHAN_PRESENT;

        assert_eq!(sb.first_inode(), 11);
        assert!(sb.has_read_only_compatible_feature(SuperBlock::FEATURE_RO_COMPAT_ORPHAN_PRESENT));
        assert_eq!(sb.last_orphan(), 0);

        sb.set_last_orphan(42);
        assert_eq!(sb.last_orphan(), 42);
    }

    #[test]
    fn final_group_uses_actual_inode_mutation_bound() {
        // Every bit pattern of SuperBlock's integer/byte fields is valid.
        let mut sb: SuperBlock = unsafe { core::mem::zeroed() };
        sb.block_count_lo = 24;
        sb.first_data_block = 0;
        sb.blocks_per_group = 8;
        sb.inodes_per_group = 8;
        sb.inode_count = 20;

        assert_eq!(sb.block_group_count(), 3);
        assert_eq!(sb.inode_count_in_group(0), 8);
        assert_eq!(sb.inode_count_in_group(1), 8);
        assert_eq!(sb.inode_count_in_group(2), 4);
        assert_eq!(sb.inode_count_in_group(3), 0);
    }
}
