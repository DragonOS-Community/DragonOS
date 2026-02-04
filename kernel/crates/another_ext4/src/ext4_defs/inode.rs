//! The Inode Table is a linear array of struct `Inode`. The table is sized to have
//! enough blocks to store at least `sb.inode_size * sb.inodes_per_group` bytes.
//!
//! The number of the block group containing an inode can be calculated as
//! `(inode_number - 1) / sb.inodes_per_group`, and the offset into the group's table is
//! `(inode_number - 1) % sb.inodes_per_group`. There is no inode 0.

use super::crc::*;
use super::AsBytes;
use super::{ExtentNode, ExtentNodeMut};
use crate::constants::*;
use crate::prelude::*;
use crate::FileType;

bitflags! {
    #[derive(PartialEq, Eq, Debug, Clone, Copy)]
    pub struct InodeMode: u16 {
        // Premission
        const PERM_MASK = 0xFFF;
        const USER_READ = 0x100;
        const USER_WRITE = 0x80;
        const USER_EXEC = 0x40;
        const GROUP_READ = 0x20;
        const GROUP_WRITE = 0x10;
        const GROUP_EXEC = 0x8;
        const OTHER_READ = 0x4;
        const OTHER_WRITE = 0x2;
        const OTHER_EXEC = 0x1;
        // File type
        const TYPE_MASK = 0xF000;
        const FIFO = 0x1000;
        const CHARDEV = 0x2000;
        const DIRECTORY = 0x4000;
        const BLOCKDEV = 0x6000;
        const FILE = 0x8000;
        const SOFTLINK = 0xA000;
        const SOCKET = 0xC000;
    }
}

impl InodeMode {
    /// Enable read, write, and execute for all users.
    pub const ALL_RWX: InodeMode = InodeMode::from_bits_retain(0o777);
    /// Enable read and write for all users.
    pub const ALL_RW: InodeMode = InodeMode::from_bits_retain(0o666);

    /// Set an inode mode from a file type and permission bits.
    pub fn from_type_and_perm(file_type: FileType, perm: InodeMode) -> Self {
        (match file_type {
            FileType::RegularFile => InodeMode::FILE,
            FileType::Directory => InodeMode::DIRECTORY,
            FileType::CharacterDev => InodeMode::CHARDEV,
            FileType::BlockDev => InodeMode::BLOCKDEV,
            FileType::Fifo => InodeMode::FIFO,
            FileType::Socket => InodeMode::SOCKET,
            FileType::SymLink => InodeMode::SOFTLINK,
            _ => InodeMode::FILE,
        }) | (perm & InodeMode::PERM_MASK)
    }
    /// Get permission bits of an inode mode.
    pub fn perm(&self) -> InodeMode {
        *self & InodeMode::PERM_MASK
    }
    /// Get the file type of an inode mode.
    pub fn file_type(&self) -> FileType {
        match *self & InodeMode::TYPE_MASK {
            InodeMode::CHARDEV => FileType::CharacterDev,
            InodeMode::DIRECTORY => FileType::Directory,
            InodeMode::BLOCKDEV => FileType::BlockDev,
            InodeMode::FILE => FileType::RegularFile,
            InodeMode::FIFO => FileType::Fifo,
            InodeMode::SOCKET => FileType::Socket,
            InodeMode::SOFTLINK => FileType::SymLink,
            _ => FileType::Unknown,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone)]
pub struct Linux2 {
    /// Upper 16-bits of the block count. See the note attached to i_blocks_lo.
    l_blocks_hi: u16,
    /// Upper 16-bits of the extended attribute block.
    l_file_acl_hi: u16,
    /// Upper 16-bits of the Owner UID.
    l_uid_hi: u16,
    /// Upper 16-bits of the GID.
    l_gid_hi: u16,
    /// Lower 16-bits of the inode checksum.
    l_checksum_lo: u16,
    /// Reserved.
    l_reserved: u16,
}

/// The I-node Structure.
///
/// In ext2 and ext3, the inode structure size was fixed at 128 bytes
/// (EXT2_GOOD_OLD_INODE_SIZE) and each inode had a disk record size of
/// 128 bytes. By default, ext4 inode records are 256 bytes, and (as of
/// October 2013) the inode structure is 156 bytes (i_extra_isize = 28).
///
/// We only implement the larger version for simplicity. Guarantee that
/// `sb.inode_size` equals to 256. This value will be checked when
/// loading the filesystem.
#[repr(C)]
#[derive(Debug, Clone)]
pub struct Inode {
    /// File mode.
    mode: u16,
    /// Lower 16-bits of Owner UID.
    uid: u16,
    /// Lower 32-bits of size in bytes.
    size: u32,
    /// Last access time, in seconds since the epoch.
    atime: u32,
    /// Last inode change time, in seconds since the epoch.
    ctime: u32,
    /// Last data modification time, in seconds since the epoch.
    mtime: u32,
    /// Deletion Time, in seconds since the epoch.
    dtime: u32,
    /// Lower 16-bits of GID.
    gid: u16,
    /// Hard link count.
    link_count: u16,
    /// Lower 32-bits of "512-byte block" count.
    block_count: u32,
    /// Inode flags.
    flags: u32,
    /// Os related fields 1.
    osd1: u32,
    /// Block map or extent tree.
    block: [u8; 60],
    /// File version (for NFS).
    generation: u32,
    /// Lower 32-bits of extended attribute block.
    file_acl: u32,
    /// Upper 32-bits of file/directory size.
    size_hi: u32,
    /// (Obsolete) fragment address.
    faddr: u32,
    /// Os related fields 2.
    osd2: Linux2,
    /// Size of this inode - 128. Alternately, the size of the extended inode
    /// fields beyond the original ext2 inode, including this field.
    extra_isize: u16,
    /// Upper 16-bits of the inode checksum.
    checksum_hi: u16,
    /// Extra change time bits. This provides sub-second precision.
    ctime_extra: u32,
    /// Extra modification time bits. This provides sub-second precision.
    mtime_extra: u32,
    /// Extra access time bits. This provides sub-second precision.
    atime_extra: u32,
    /// File creation time, in seconds since the epoch.
    crtime: u32,
    /// Extra file creation time bits. This provides sub-second precision.
    crtime_extra: u32,
    /// Upper 32-bits for version number.
    version_hi: u32,
    /// Project id
    projid: u32,
}

/// Because `[u8; 60]` cannot derive `Default`, we implement it manually.
impl Default for Inode {
    fn default() -> Self {
        Self {
            mode: 0,
            uid: 0,
            size: 0,
            atime: 0,
            ctime: 0,
            mtime: 0,
            dtime: 0,
            gid: 0,
            link_count: 0,
            block_count: 0,
            flags: 0,
            osd1: 0,
            block: [0u8; 60],
            generation: 0,
            file_acl: 0,
            size_hi: 0,
            faddr: 0,
            osd2: Linux2 {
                l_blocks_hi: 0,
                l_file_acl_hi: 0,
                l_uid_hi: 0,
                l_gid_hi: 0,
                l_checksum_lo: 0,
                l_reserved: 0,
            },
            extra_isize: (core::mem::size_of::<Inode>() - 128) as u16,
            checksum_hi: 0,
            ctime_extra: 0,
            mtime_extra: 0,
            atime_extra: 0,
            crtime: 0,
            crtime_extra: 0,
            version_hi: 0,
            projid: 0,
        }
    }
}

unsafe impl AsBytes for Inode {}

impl Inode {
    const FLAG_EXTENTS: u32 = 0x00080000;

    pub fn mode(&self) -> InodeMode {
        InodeMode::from_bits_truncate(self.mode)
    }

    pub fn set_mode(&mut self, mode: InodeMode) {
        self.mode = mode.bits();
    }

    pub fn file_type(&self) -> FileType {
        self.mode().file_type()
    }

    pub fn is_file(&self) -> bool {
        self.file_type() == FileType::RegularFile
    }

    pub fn is_dir(&self) -> bool {
        self.file_type() == FileType::Directory
    }

    pub fn is_softlink(&self) -> bool {
        self.file_type() == FileType::SymLink
    }

    /// Return the inline i_block area (60 bytes).
    ///
    /// - For ext4, this area can store either extent root or fast symlink link content.
    pub fn inline_block(&self) -> &[u8; 60] {
        &self.block
    }

    /// Mutable inline i_block area (60 bytes).
    pub fn inline_block_mut(&mut self) -> &mut [u8; 60] {
        &mut self.block
    }

    pub fn perm(&self) -> InodeMode {
        self.mode().perm()
    }

    pub fn link_count(&self) -> u16 {
        self.link_count
    }

    pub fn set_link_count(&mut self, cnt: u16) {
        self.link_count = cnt;
    }

    pub fn uid(&self) -> u32 {
        self.uid as u32 | ((self.osd2.l_uid_hi as u32) << 16)
    }

    pub fn set_uid(&mut self, uid: u32) {
        self.uid = uid as u16;
        self.osd2.l_uid_hi = (uid >> 16) as u16;
    }

    pub fn gid(&self) -> u32 {
        self.gid as u32 | ((self.osd2.l_gid_hi as u32) << 16)
    }

    pub fn set_gid(&mut self, gid: u32) {
        self.gid = gid as u16;
        self.osd2.l_gid_hi = (gid >> 16) as u16;
    }

    pub fn size(&self) -> u64 {
        self.size as u64 | ((self.size_hi as u64) << 32)
    }

    pub fn set_size(&mut self, size: u64) {
        self.size = ((size << 32) >> 32) as u32;
        self.size_hi = (size >> 32) as u32;
    }

    pub fn atime(&self) -> u32 {
        self.atime
    }

    pub fn set_atime(&mut self, atime: u32) {
        self.atime = atime;
    }

    pub fn ctime(&self) -> u32 {
        self.ctime
    }

    pub fn set_ctime(&mut self, ctime: u32) {
        self.ctime = ctime;
    }

    pub fn mtime(&self) -> u32 {
        self.mtime
    }

    pub fn set_mtime(&mut self, mtime: u32) {
        self.mtime = mtime;
    }

    pub fn dtime(&self) -> u32 {
        self.dtime
    }

    pub fn set_dtime(&mut self, dtime: u32) {
        self.dtime = dtime;
    }

    pub fn crtime(&self) -> u32 {
        self.crtime
    }

    pub fn set_crtime(&mut self, crtime: u32) {
        self.crtime = crtime;
    }

    /// Get the number of 512-byte blocks (`INODE_BLOCK_SIZE`) used by the inode.
    ///
    /// WARN: This is different from filesystem block (`BLOCK_SIZE`)!
    pub fn block_count(&self) -> u64 {
        self.block_count as u64 | ((self.osd2.l_blocks_hi as u64) << 32)
    }

    /// Get the number of filesystem blocks (`BLOCK_SIZE`) used by the inode.
    pub fn fs_block_count(&self) -> u64 {
        self.block_count() * INODE_BLOCK_SIZE as u64 / BLOCK_SIZE as u64
    }

    /// Set the number of 512-byte blocks (`INODE_BLOCK_SIZE`) used by the inode.
    ///
    /// WARN: This is different from filesystem block (`BLOCK_SIZE`)!
    pub fn set_block_count(&mut self, cnt: u64) {
        self.block_count = cnt as u32;
        self.osd2.l_blocks_hi = (cnt >> 32) as u16;
    }

    /// Set the number of filesystem blocks (`BLOCK_SIZE`) used by the inode.
    pub fn set_fs_block_count(&mut self, cnt: u64) {
        self.set_block_count(cnt * BLOCK_SIZE as u64 / INODE_BLOCK_SIZE as u64);
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }

    pub fn set_generation(&mut self, generation: u32) {
        self.generation = generation;
    }

    pub fn flags(&self) -> u32 {
        self.flags
    }

    pub fn set_flags(&mut self, f: u32) {
        self.flags |= f;
    }

    pub fn xattr_block(&self) -> PBlockId {
        ((self.osd2.l_file_acl_hi as u64) << 32) | self.file_acl as u64
    }

    pub fn set_xattr_block(&mut self, block: PBlockId) {
        self.file_acl = block as u32;
        self.osd2.l_file_acl_hi = (block >> 32) as u16;
    }

    /* Extent methods */

    /// Get the immutable extent root node
    pub fn extent_root(&self) -> ExtentNode<'_> {
        ExtentNode::from_bytes(unsafe { core::slice::from_raw_parts(self.block.as_ptr(), 60) })
    }

    /// Get the mutable extent root node
    pub fn extent_root_mut(&mut self) -> ExtentNodeMut<'_> {
        ExtentNodeMut::from_bytes(unsafe {
            core::slice::from_raw_parts_mut(self.block.as_mut_ptr(), 60)
        })
    }

    /// Initialize the `flags` and `block` field of inode. Mark the
    /// inode to use extent for block mapping. Initialize the root
    /// node of the extent tree
    pub fn extent_init(&mut self) {
        self.set_flags(Self::FLAG_EXTENTS);
        self.extent_root_mut().init(0, 0);
    }

    /// Check if this inode is a device node (character or block device).
    pub fn is_device(&self) -> bool {
        matches!(
            self.file_type(),
            FileType::CharacterDev | FileType::BlockDev
        )
    }

    /// Get device number (major, minor) for character/block device nodes.
    ///
    /// Linux ext4 stores device numbers in i_block[0..1]:
    /// - If i_block[0] != 0: old format, decode from i_block[0]
    /// - If i_block[0] == 0: new format, decode from i_block[1]
    pub fn device(&self) -> (u32, u32) {
        let block0 =
            u32::from_le_bytes([self.block[0], self.block[1], self.block[2], self.block[3]]);
        let block1 =
            u32::from_le_bytes([self.block[4], self.block[5], self.block[6], self.block[7]]);

        if block0 != 0 {
            device::old_decode_dev(block0)
        } else {
            device::new_decode_dev(block1)
        }
    }

    /// Set device number (major, minor) for character/block device nodes.
    ///
    /// This stores the device number in i_block[0..1] using Linux ext4 format:
    /// - Old format if major < 256 && minor < 256
    /// - New format otherwise
    ///
    /// Note: This should only be called for device inodes, and extent_init()
    /// should NOT be called for device inodes.
    pub fn set_device(&mut self, major: u32, minor: u32) {
        // Clear i_block area
        self.block.fill(0);

        if device::old_valid_dev(major, minor) {
            // Old format: store in i_block[0]
            let encoded = device::old_encode_dev(major, minor);
            self.block[0..4].copy_from_slice(&encoded.to_le_bytes());
        } else {
            // New format: i_block[0] = 0, store in i_block[1]
            let encoded = device::new_encode_dev(major, minor);
            self.block[4..8].copy_from_slice(&encoded.to_le_bytes());
        }
    }
}

/// A combination of an `Inode` and its id
#[derive(Clone, Debug)]
pub struct InodeRef {
    pub id: InodeId,
    pub inode: Box<Inode>,
}

impl InodeRef {
    pub fn new(id: InodeId, inode: Box<Inode>) -> Self {
        Self { id, inode }
    }

    pub fn set_checksum(&mut self, uuid: &[u8]) {
        // Must set checksum field to 0 before calculation to avoid including old value
        // causing checksum to never match (Linux semantics).
        self.inode.osd2.l_checksum_lo = 0;
        self.inode.checksum_hi = 0;
        let mut checksum = crc32(CRC32_INIT, uuid);
        checksum = crc32(checksum, &self.id.to_le_bytes());
        checksum = crc32(checksum, &self.inode.generation.to_le_bytes());
        checksum = crc32(checksum, self.inode.to_bytes());
        self.inode.osd2.l_checksum_lo = checksum as u16;
        self.inode.checksum_hi = (checksum >> 16) as u16;
    }
}

#[derive(Debug, Clone)]
pub struct FileAttr {
    pub ino: InodeId,
    pub size: u64,
    pub atime: u32,
    pub mtime: u32,
    pub ctime: u32,
    pub crtime: u32,
    pub blocks: u64,
    pub ftype: FileType,
    pub perm: InodeMode,
    pub links: u16,
    pub uid: u32,
    pub gid: u32,
    /// Device number (major, minor) for character/block devices.
    /// Only meaningful when ftype is CharacterDev or BlockDev.
    pub rdev: (u32, u32),
}

/// Device number encoding/decoding utilities compatible with Linux ext4.
///
/// Linux ext4 stores device numbers in i_block[0..1]:
/// - Old format: i_block[0] contains encoded dev (major < 256 && minor < 256)
/// - New format: i_block[0] = 0, i_block[1] contains encoded dev
pub mod device {
    /// Check if device number fits in old format (major < 256 && minor < 256).
    #[inline]
    pub const fn old_valid_dev(major: u32, minor: u32) -> bool {
        major < 256 && minor < 256
    }

    /// Encode device number in old format (16-bit).
    /// Layout: [major(8-bit)][minor(8-bit)]
    #[inline]
    pub const fn old_encode_dev(major: u32, minor: u32) -> u32 {
        ((major & 0xff) << 8) | (minor & 0xff)
    }

    /// Decode device number from old format.
    #[inline]
    pub const fn old_decode_dev(dev: u32) -> (u32, u32) {
        let major = (dev >> 8) & 0xff;
        let minor = dev & 0xff;
        (major, minor)
    }

    /// Encode device number in new format (32-bit).
    /// Layout: [minor_lo(8-bit)][major(12-bit)][minor_hi(12-bit)]
    #[inline]
    pub const fn new_encode_dev(major: u32, minor: u32) -> u32 {
        (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12)
    }

    /// Decode device number from new format.
    #[inline]
    pub const fn new_decode_dev(dev: u32) -> (u32, u32) {
        let major = (dev & 0xfff00) >> 8;
        let minor = (dev & 0xff) | ((dev >> 12) & 0xfff00);
        (major, minor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ==================== device module tests ====================

    mod device_encoding {
        use super::device::*;

        #[test]
        fn test_old_valid_dev_boundary() {
            // Valid old format
            assert!(old_valid_dev(0, 0));
            assert!(old_valid_dev(1, 3)); // /dev/null
            assert!(old_valid_dev(255, 255)); // max old format

            // Invalid old format
            assert!(!old_valid_dev(256, 0));
            assert!(!old_valid_dev(0, 256));
            assert!(!old_valid_dev(256, 256));
            assert!(!old_valid_dev(1000, 1000));
        }

        #[test]
        fn test_old_encode_decode_roundtrip() {
            let test_cases = [
                (0, 0),
                (1, 3),     // /dev/null
                (1, 5),     // /dev/zero
                (4, 0),     // /dev/tty0
                (8, 0),     // /dev/sda
                (8, 1),     // /dev/sda1
                (255, 255), // max
            ];

            for (major, minor) in test_cases {
                let encoded = old_encode_dev(major, minor);
                let (dec_major, dec_minor) = old_decode_dev(encoded);
                assert_eq!(
                    (dec_major, dec_minor),
                    (major, minor),
                    "old format roundtrip failed for ({}, {})",
                    major,
                    minor
                );
            }
        }

        #[test]
        fn test_old_encode_known_values() {
            // /dev/null: major=1, minor=3 -> 0x0103
            assert_eq!(old_encode_dev(1, 3), 0x0103);
            // /dev/zero: major=1, minor=5 -> 0x0105
            assert_eq!(old_encode_dev(1, 5), 0x0105);
            // /dev/sda: major=8, minor=0 -> 0x0800
            assert_eq!(old_encode_dev(8, 0), 0x0800);
        }

        #[test]
        fn test_new_encode_decode_roundtrip() {
            let test_cases = [
                (0, 0),
                (1, 256),        // minor exceeds old format
                (256, 0),        // major exceeds old format
                (256, 256),      // both exceed
                (8, 65536),      // large minor
                (259, 65536),    // virtio-blk style
                (4095, 1048575), // near max (12-bit major, 20-bit minor)
            ];

            for (major, minor) in test_cases {
                let encoded = new_encode_dev(major, minor);
                let (dec_major, dec_minor) = new_decode_dev(encoded);
                assert_eq!(
                    (dec_major, dec_minor),
                    (major, minor),
                    "new format roundtrip failed for ({}, {})",
                    major,
                    minor
                );
            }
        }

        #[test]
        fn test_new_format_also_works_for_small_values() {
            // New format should also correctly handle small values
            let test_cases = [(1, 3), (8, 0), (255, 255)];

            for (major, minor) in test_cases {
                let encoded = new_encode_dev(major, minor);
                let (dec_major, dec_minor) = new_decode_dev(encoded);
                assert_eq!((dec_major, dec_minor), (major, minor));
            }
        }

        #[test]
        fn test_format_discrimination() {
            // When old_valid_dev returns true, old format should be used
            // When old_valid_dev returns false, new format should be used
            // This tests the format selection logic

            // Small device: should use old format
            let (major, minor) = (1, 3);
            assert!(old_valid_dev(major, minor));

            // Large device: must use new format
            let (major, minor) = (8, 256);
            assert!(!old_valid_dev(major, minor));
        }
    }

    // ==================== Inode device methods tests ====================

    mod inode_device {
        use super::*;

        fn create_test_inode() -> Inode {
            Inode::default()
        }

        #[test]
        fn test_is_device() {
            let mut inode = create_test_inode();

            // Regular file
            inode.set_mode(InodeMode::FILE | InodeMode::ALL_RW);
            assert!(!inode.is_device());

            // Directory
            inode.set_mode(InodeMode::DIRECTORY | InodeMode::ALL_RWX);
            assert!(!inode.is_device());

            // Character device
            inode.set_mode(InodeMode::CHARDEV | InodeMode::ALL_RW);
            assert!(inode.is_device());

            // Block device
            inode.set_mode(InodeMode::BLOCKDEV | InodeMode::ALL_RW);
            assert!(inode.is_device());

            // FIFO
            inode.set_mode(InodeMode::FIFO | InodeMode::ALL_RW);
            assert!(!inode.is_device());

            // Socket
            inode.set_mode(InodeMode::SOCKET | InodeMode::ALL_RW);
            assert!(!inode.is_device());

            // Symlink
            inode.set_mode(InodeMode::SOFTLINK | InodeMode::ALL_RWX);
            assert!(!inode.is_device());
        }

        #[test]
        fn test_set_device_old_format() {
            let mut inode = create_test_inode();

            // /dev/null: major=1, minor=3
            inode.set_device(1, 3);

            // Verify i_block[0] is non-zero (old format)
            let block0 = u32::from_le_bytes([
                inode.block[0],
                inode.block[1],
                inode.block[2],
                inode.block[3],
            ]);
            assert_ne!(block0, 0, "Old format should have non-zero i_block[0]");

            // Verify i_block[1] is zero
            let block1 = u32::from_le_bytes([
                inode.block[4],
                inode.block[5],
                inode.block[6],
                inode.block[7],
            ]);
            assert_eq!(block1, 0, "Old format should have zero i_block[1]");

            // Verify roundtrip
            let (major, minor) = inode.device();
            assert_eq!((major, minor), (1, 3));
        }

        #[test]
        fn test_set_device_new_format() {
            let mut inode = create_test_inode();

            // Device with minor=256 (exceeds old format)
            inode.set_device(8, 256);

            // Verify i_block[0] is zero (new format marker)
            let block0 = u32::from_le_bytes([
                inode.block[0],
                inode.block[1],
                inode.block[2],
                inode.block[3],
            ]);
            assert_eq!(block0, 0, "New format should have zero i_block[0]");

            // Verify i_block[1] is non-zero
            let block1 = u32::from_le_bytes([
                inode.block[4],
                inode.block[5],
                inode.block[6],
                inode.block[7],
            ]);
            assert_ne!(block1, 0, "New format should have non-zero i_block[1]");

            // Verify roundtrip
            let (major, minor) = inode.device();
            assert_eq!((major, minor), (8, 256));
        }

        #[test]
        fn test_set_device_clears_block() {
            let mut inode = create_test_inode();

            // Fill block with garbage
            inode.block.fill(0xff);

            // Set device
            inode.set_device(1, 3);

            // Verify rest of block is cleared
            for i in 8..60 {
                assert_eq!(
                    inode.block[i], 0,
                    "block[{}] should be cleared after set_device",
                    i
                );
            }
        }

        #[test]
        fn test_device_roundtrip_various() {
            let mut inode = create_test_inode();

            let test_cases = [
                (1, 3),       // /dev/null (old format)
                (1, 5),       // /dev/zero (old format)
                (4, 64),      // /dev/ttyS0 (old format)
                (8, 0),       // /dev/sda (old format)
                (8, 16),      // /dev/sdb (old format)
                (8, 256),     // minor > 255 (new format)
                (254, 0),     // virtio-blk (old format)
                (259, 0),     // nvme (new format, major > 255)
                (259, 65536), // nvme with large minor (new format)
            ];

            for (major, minor) in test_cases {
                inode.set_device(major, minor);
                let (got_major, got_minor) = inode.device();
                assert_eq!(
                    (got_major, got_minor),
                    (major, minor),
                    "Device roundtrip failed for ({}, {})",
                    major,
                    minor
                );
            }
        }

        #[test]
        fn test_device_vs_extent_exclusivity() {
            let mut inode = create_test_inode();

            // Set as device
            inode.set_device(1, 3);
            let device_before = inode.device();

            // Now initialize extent (this would corrupt device data!)
            // This test documents the expected behavior: don't call extent_init on device inodes
            inode.extent_init();

            // After extent_init, device() will return garbage
            // This is expected - device inodes should never have extent_init called
            let device_after = inode.device();

            // The values will differ, demonstrating mutual exclusivity
            // In real usage, we must ensure device inodes never call extent_init
            assert_ne!(
                device_before, device_after,
                "extent_init should corrupt device data (this is expected)"
            );
        }
    }

    // ==================== FileAttr tests ====================

    mod file_attr {
        use super::*;

        #[test]
        fn test_file_attr_rdev_field() {
            let attr = FileAttr {
                ino: 123,
                size: 0,
                atime: 0,
                mtime: 0,
                ctime: 0,
                crtime: 0,
                blocks: 0,
                ftype: FileType::CharacterDev,
                perm: InodeMode::ALL_RW,
                links: 1,
                uid: 0,
                gid: 0,
                rdev: (1, 3), // /dev/null
            };

            assert_eq!(attr.rdev, (1, 3));
            assert_eq!(attr.ftype, FileType::CharacterDev);
        }

        #[test]
        fn test_file_attr_rdev_default_for_regular() {
            let attr = FileAttr {
                ino: 456,
                size: 1024,
                atime: 0,
                mtime: 0,
                ctime: 0,
                crtime: 0,
                blocks: 2,
                ftype: FileType::RegularFile,
                perm: InodeMode::ALL_RW,
                links: 1,
                uid: 0,
                gid: 0,
                rdev: (0, 0), // Not a device
            };

            assert_eq!(attr.rdev, (0, 0));
            assert_eq!(attr.ftype, FileType::RegularFile);
        }
    }

    // ==================== Linux compatibility tests ====================

    mod linux_compat {
        use super::device::*;

        /// Test values known from Linux systems
        #[test]
        fn test_linux_known_devices() {
            // These are actual device numbers from a typical Linux system
            let devices = [
                ("null", 1, 3),
                ("zero", 1, 5),
                ("full", 1, 7),
                ("random", 1, 8),
                ("urandom", 1, 9),
                ("tty", 5, 0),
                ("console", 5, 1),
                ("sda", 8, 0),
                ("sda1", 8, 1),
                ("sdb", 8, 16),
                ("loop0", 7, 0),
                ("loop1", 7, 1),
            ];

            for (name, major, minor) in devices {
                // All these fit in old format
                assert!(
                    old_valid_dev(major, minor),
                    "{} should fit in old format",
                    name
                );

                // Roundtrip test
                let encoded = old_encode_dev(major, minor);
                let (dec_maj, dec_min) = old_decode_dev(encoded);
                assert_eq!(
                    (dec_maj, dec_min),
                    (major, minor),
                    "Roundtrip failed for {}",
                    name
                );
            }
        }

        /// Test encoding matches Linux kernel's new_encode_dev
        #[test]
        fn test_new_encode_matches_linux() {
            // Linux kernel: (minor & 0xff) | (major << 8) | ((minor & ~0xff) << 12)
            // Test with known values

            // major=1, minor=256
            // Linux: (256 & 0xff) | (1 << 8) | ((256 & ~0xff) << 12)
            //      = 0 | 0x100 | (0x100 << 12)
            //      = 0x100 | 0x100000
            //      = 0x100100
            let encoded = new_encode_dev(1, 256);
            assert_eq!(encoded, 0x100100);

            // major=259, minor=0
            // Linux: (0 & 0xff) | (259 << 8) | ((0 & ~0xff) << 12)
            //      = 0 | 0x10300 | 0
            //      = 0x10300
            let encoded = new_encode_dev(259, 0);
            assert_eq!(encoded, 0x10300);
        }
    }
}
