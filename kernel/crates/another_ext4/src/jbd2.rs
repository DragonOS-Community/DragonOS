//! JBD2 on-disk format primitives.
//!
//! JBD2 is deliberately kept separate from the ext4 transaction engine: this
//! module only decodes, validates and checksums bytes written by Linux JBD2.
//! All integer fields in the journal are big-endian (unlike ext4 metadata).

use crate::{error::Ext4Error, ext4_defs::crc::crc32, prelude::Vec, ErrCode};

pub const MAGIC: u32 = 0xc03b_3998;
pub const SUPERBLOCK_BYTES: usize = 1024;
pub const HEADER_BYTES: usize = 12;
pub const BLOCK_TAIL_BYTES: usize = 4;

pub const FEATURE_COMPAT_CHECKSUM: u32 = 0x0000_0001;
pub const FEATURE_INCOMPAT_REVOKE: u32 = 0x0000_0001;
pub const FEATURE_INCOMPAT_64BIT: u32 = 0x0000_0002;
pub const FEATURE_INCOMPAT_ASYNC_COMMIT: u32 = 0x0000_0004;
pub const FEATURE_INCOMPAT_CSUM_V2: u32 = 0x0000_0008;
pub const FEATURE_INCOMPAT_CSUM_V3: u32 = 0x0000_0010;
pub const FEATURE_INCOMPAT_FAST_COMMIT: u32 = 0x0000_0020;

pub const FLAG_ESCAPE: u32 = 1;
pub const FLAG_SAME_UUID: u32 = 2;
pub const FLAG_DELETED: u32 = 4;
pub const FLAG_LAST_TAG: u32 = 8;
const KNOWN_TAG_FLAGS: u32 = FLAG_ESCAPE | FLAG_SAME_UUID | FLAG_DELETED | FLAG_LAST_TAG;
pub const CRC32C_CHKSUM: u8 = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum BlockType {
    Descriptor = 1,
    Commit = 2,
    SuperblockV1 = 3,
    SuperblockV2 = 4,
    Revoke = 5,
}

impl BlockType {
    fn from_raw(raw: u32) -> Result<Self, Ext4Error> {
        match raw {
            1 => Ok(Self::Descriptor),
            2 => Ok(Self::Commit),
            3 => Ok(Self::SuperblockV1),
            4 => Ok(Self::SuperblockV2),
            5 => Ok(Self::Revoke),
            _ => malformed("unknown JBD2 block type"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Header {
    pub block_type: BlockType,
    pub sequence: u32,
}

impl Header {
    pub fn parse(bytes: &[u8]) -> Result<Self, Ext4Error> {
        need(bytes, HEADER_BYTES)?;
        if be32(bytes, 0)? != MAGIC {
            return malformed("invalid JBD2 magic");
        }
        Ok(Self {
            block_type: BlockType::from_raw(be32(bytes, 4)?)?,
            sequence: be32(bytes, 8)?,
        })
    }

    pub fn encode_into(&self, bytes: &mut [u8]) -> Result<(), Ext4Error> {
        need(bytes, HEADER_BYTES)?;
        put32(bytes, 0, MAGIC)?;
        put32(bytes, 4, self.block_type as u32)?;
        put32(bytes, 8, self.sequence)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChecksumMode {
    /// Old journals without any checksum feature.
    None,
    /// Linux checksum v3: full CRC32C tags and block tails.
    V3,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Features {
    pub checksum: ChecksumMode,
    pub has_64bit: bool,
    pub has_revoke: bool,
}

impl Features {
    /// Validate the intentionally narrow feature matrix supported by the
    /// synchronous writer/recovery design.  V1/V2 checksums, async commit and
    /// fast commit are rejected rather than being silently misinterpreted.
    pub fn validate(compat: u32, incompat: u32, ro_compat: u32) -> Result<Self, Ext4Error> {
        if ro_compat != 0 {
            return unsupported("JBD2 read-only-compatible features are unsupported");
        }
        if compat != 0 {
            return unsupported("JBD2 checksum v1 or unknown compatible feature");
        }
        let allowed = FEATURE_INCOMPAT_REVOKE | FEATURE_INCOMPAT_64BIT | FEATURE_INCOMPAT_CSUM_V3;
        if incompat & !allowed != 0 {
            return unsupported("unsupported JBD2 incompatible feature");
        }
        let checksum = if incompat & FEATURE_INCOMPAT_CSUM_V3 != 0 {
            ChecksumMode::V3
        } else {
            ChecksumMode::None
        };
        Ok(Self {
            checksum,
            has_64bit: incompat & FEATURE_INCOMPAT_64BIT != 0,
            has_revoke: incompat & FEATURE_INCOMPAT_REVOKE != 0,
        })
    }

    pub const fn tag_bytes(self) -> usize {
        match (self.checksum, self.has_64bit) {
            (ChecksumMode::V3, true) => 16,
            // Feature validation makes this combination unreachable.  Keep
            // the Linux on-disk size here so manually constructed values
            // cannot cause a short parse.
            (ChecksumMode::V3, false) => 16,
            (ChecksumMode::None, true) => 12,
            (ChecksumMode::None, false) => 8,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Superblock {
    pub block_size: u32,
    pub max_len: u32,
    pub first: u32,
    pub sequence: u32,
    pub start: u32,
    pub errno: u32,
    pub features: Features,
    pub uuid: [u8; 16],
    pub checksum_type: u8,
}

impl Superblock {
    pub fn parse(bytes: &[u8], expected_block_size: u32) -> Result<Self, Ext4Error> {
        need(bytes, SUPERBLOCK_BYTES)?;
        let header = Header::parse(bytes)?;
        if header.block_type != BlockType::SuperblockV2 {
            return unsupported("only JBD2 superblock v2 is supported");
        }
        let block_size = be32(bytes, 12)?;
        let max_len = be32(bytes, 16)?;
        let first = be32(bytes, 20)?;
        let start = be32(bytes, 28)?;
        if block_size != expected_block_size || block_size < SUPERBLOCK_BYTES as u32 {
            return unsupported("unsupported JBD2 block size");
        }
        if max_len <= first || first == 0 || start >= max_len || (start != 0 && start < first) {
            return malformed("JBD2 journal ring is out of range");
        }
        let features = Features::validate(be32(bytes, 36)?, be32(bytes, 40)?, be32(bytes, 44)?)?;
        let checksum_type = bytes[80];
        if features.checksum == ChecksumMode::V3 {
            if checksum_type != CRC32C_CHKSUM {
                return unsupported("JBD2 checksum v3 is not CRC32C");
            }
            if superblock_checksum(bytes)? != be32(bytes, 252)? {
                return malformed("invalid JBD2 superblock checksum");
            }
        } else if checksum_type != 0 {
            return unsupported("legacy JBD2 journal declares a checksum type");
        }
        let mut uuid = [0; 16];
        uuid.copy_from_slice(&bytes[48..64]);
        Ok(Self {
            block_size,
            max_len,
            first,
            sequence: be32(bytes, 24)?,
            start,
            errno: be32(bytes, 32)?,
            features,
            uuid,
            checksum_type,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DescriptorTag {
    pub block: u64,
    pub flags: u32,
    pub checksum: Option<u32>,
}

pub struct DescriptorTags<'a> {
    bytes: &'a [u8],
    features: Features,
    offset: usize,
    end: usize,
    target_blocks: u64,
    finished: bool,
}

impl<'a> DescriptorTags<'a> {
    pub fn parse(
        block: &'a [u8],
        features: Features,
        expected_sequence: u32,
        target_blocks: u64,
    ) -> Result<Self, Ext4Error> {
        let header = Header::parse(block)?;
        if header.block_type != BlockType::Descriptor || header.sequence != expected_sequence {
            return malformed("unexpected JBD2 descriptor header");
        }
        let tail = if features.checksum == ChecksumMode::V3 {
            BLOCK_TAIL_BYTES
        } else {
            0
        };
        if block.len() < HEADER_BYTES + features.tag_bytes() + tail {
            return malformed("JBD2 descriptor has no complete tag");
        }
        Ok(Self {
            bytes: block,
            features,
            offset: HEADER_BYTES,
            end: block.len() - tail,
            target_blocks,
            finished: false,
        })
    }

    pub fn next_tag(&mut self) -> Result<Option<DescriptorTag>, Ext4Error> {
        if self.finished {
            return Ok(None);
        }
        let size = self.features.tag_bytes();
        let tag_end = self.offset.checked_add(size).ok_or_else(eio)?;
        if tag_end > self.end {
            return malformed("unterminated JBD2 descriptor tags");
        }
        let low = be32(self.bytes, self.offset)? as u64;
        let (flags, high, checksum) = match self.features.checksum {
            ChecksumMode::V3 => {
                let flags = be32(self.bytes, self.offset + 4)?;
                let encoded_high = be32(self.bytes, self.offset + 8)? as u64;
                if !self.features.has_64bit && encoded_high != 0 {
                    return malformed("32-bit JBD2 tag has a non-zero high block number");
                }
                let high = if self.features.has_64bit {
                    encoded_high
                } else {
                    0
                };
                (flags, high, Some(be32(self.bytes, self.offset + 12)?))
            }
            ChecksumMode::None => {
                let flags = be16(self.bytes, self.offset + 6)? as u32;
                let high = if self.features.has_64bit {
                    be32(self.bytes, self.offset + 8)? as u64
                } else {
                    0
                };
                (flags, high, None)
            }
        };
        if flags & !KNOWN_TAG_FLAGS != 0 {
            return malformed("unknown JBD2 descriptor tag flag");
        }
        self.offset = tag_end;
        if flags & FLAG_SAME_UUID == 0 {
            self.offset = self.offset.checked_add(16).ok_or_else(eio)?;
            if self.offset > self.end {
                return malformed("truncated JBD2 descriptor UUID");
            }
        }
        let block = low | (high << 32);
        if block >= self.target_blocks {
            return malformed("JBD2 descriptor target block is out of range");
        }
        self.finished = flags & FLAG_LAST_TAG != 0;
        Ok(Some(DescriptorTag {
            block,
            flags,
            checksum,
        }))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommitHeader {
    pub sequence: u32,
    pub checksum: Option<u32>,
    pub commit_sec: u64,
    pub commit_nsec: u32,
}

impl CommitHeader {
    pub fn parse(
        block: &[u8],
        features: Features,
        expected_sequence: u32,
        checksum_seed: u32,
    ) -> Result<Self, Ext4Error> {
        need(block, 60)?;
        let header = Header::parse(block)?;
        if header.block_type != BlockType::Commit || header.sequence != expected_sequence {
            return malformed("unexpected JBD2 commit header");
        }
        let checksum = if features.checksum == ChecksumMode::V3 {
            if commit_checksum(checksum_seed, block)? != be32(block, 16)? {
                return malformed("invalid JBD2 commit checksum");
            }
            Some(be32(block, 16)?)
        } else {
            None
        };
        Ok(Self {
            sequence: header.sequence,
            checksum,
            commit_sec: be64(block, 48)?,
            commit_nsec: be32(block, 56)?,
        })
    }
}

/// Parse all revoke records after checking header, count, alignment and range.
pub fn parse_revoke_records(
    block: &[u8],
    features: Features,
    expected_sequence: u32,
    target_blocks: u64,
) -> Result<Vec<u64>, Ext4Error> {
    if !features.has_revoke {
        return unsupported("JBD2 revoke block without revoke feature");
    }
    let header = Header::parse(block)?;
    if header.block_type != BlockType::Revoke || header.sequence != expected_sequence {
        return malformed("unexpected JBD2 revoke header");
    }
    let count = be32(block, 12)? as usize;
    let tail = if features.checksum == ChecksumMode::V3 {
        BLOCK_TAIL_BYTES
    } else {
        0
    };
    if count < 16 || count > block.len().saturating_sub(tail) {
        return malformed("JBD2 revoke byte count is out of range");
    }
    let width = if features.has_64bit { 8 } else { 4 };
    if !(count - 16).is_multiple_of(width) {
        return malformed("misaligned JBD2 revoke records");
    }
    let mut records = Vec::with_capacity((count - 16) / width);
    let mut offset = 16;
    while offset < count {
        let record = if width == 8 {
            be64(block, offset)?
        } else {
            be32(block, offset)? as u64
        };
        if record >= target_blocks {
            return malformed("JBD2 revoke target block is out of range");
        }
        records.push(record);
        offset += width;
    }
    Ok(records)
}

pub fn superblock_checksum(bytes: &[u8]) -> Result<u32, Ext4Error> {
    need(bytes, SUPERBLOCK_BYTES)?;
    let mut checksum = crc32(u32::MAX, &bytes[..252]);
    checksum = crc32(checksum, &[0; 4]);
    Ok(crc32(checksum, &bytes[256..SUPERBLOCK_BYTES]))
}

pub fn block_checksum(seed: u32, block: &[u8]) -> Result<u32, Ext4Error> {
    if block.len() < BLOCK_TAIL_BYTES {
        return malformed("short JBD2 checksummed block");
    }
    let tail = block.len() - BLOCK_TAIL_BYTES;
    let mut checksum = crc32(seed, &block[..tail]);
    checksum = crc32(checksum, &[0; 4]);
    Ok(checksum)
}

pub fn verify_block_checksum(seed: u32, block: &[u8]) -> Result<(), Ext4Error> {
    let provided = be32(block, block.len().checked_sub(4).ok_or_else(eio)?)?;
    if block_checksum(seed, block)? != provided {
        return malformed("invalid JBD2 block checksum");
    }
    Ok(())
}

pub fn tag_checksum(seed: u32, sequence: u32, data_block: &[u8]) -> u32 {
    let checksum = crc32(seed, &sequence.to_be_bytes());
    crc32(checksum, data_block)
}

/// Linux initializes `journal->j_csum_seed` as crc32c(~0, journal UUID).
pub fn checksum_seed(uuid: &[u8; 16]) -> u32 {
    crc32(u32::MAX, uuid)
}

pub fn commit_checksum(seed: u32, block: &[u8]) -> Result<u32, Ext4Error> {
    need(block, 20)?;
    let mut checksum = crc32(seed, &block[..16]);
    checksum = crc32(checksum, &[0; 4]);
    Ok(crc32(checksum, &block[20..]))
}

fn need(bytes: &[u8], len: usize) -> Result<(), Ext4Error> {
    if bytes.len() < len {
        malformed("truncated JBD2 structure")
    } else {
        Ok(())
    }
}
fn be16(bytes: &[u8], off: usize) -> Result<u16, Ext4Error> {
    let value = bytes
        .get(off..off.checked_add(2).ok_or_else(eio)?)
        .ok_or_else(eio)?;
    Ok(u16::from_be_bytes([value[0], value[1]]))
}
fn be32(bytes: &[u8], off: usize) -> Result<u32, Ext4Error> {
    let value = bytes
        .get(off..off.checked_add(4).ok_or_else(eio)?)
        .ok_or_else(eio)?;
    Ok(u32::from_be_bytes(value.try_into().map_err(|_| eio())?))
}
fn be64(bytes: &[u8], off: usize) -> Result<u64, Ext4Error> {
    let value = bytes
        .get(off..off.checked_add(8).ok_or_else(eio)?)
        .ok_or_else(eio)?;
    Ok(u64::from_be_bytes(value.try_into().map_err(|_| eio())?))
}
fn put32(bytes: &mut [u8], off: usize, value: u32) -> Result<(), Ext4Error> {
    let dst = bytes
        .get_mut(off..off.checked_add(4).ok_or_else(eio)?)
        .ok_or_else(eio)?;
    dst.copy_from_slice(&value.to_be_bytes());
    Ok(())
}
fn eio() -> Ext4Error {
    Ext4Error::new(ErrCode::EIO)
}
fn malformed<T>(_message: &'static str) -> Result<T, Ext4Error> {
    Err(eio())
}
fn unsupported<T>(_message: &'static str) -> Result<T, Ext4Error> {
    Err(Ext4Error::new(ErrCode::ENOTSUP))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(block: &mut [u8], kind: BlockType, sequence: u32) {
        Header {
            block_type: kind,
            sequence,
        }
        .encode_into(block)
        .unwrap();
    }

    #[test]
    fn header_uses_linux_big_endian_bytes() {
        let mut bytes = [0; 12];
        header(&mut bytes, BlockType::Descriptor, 0x0102_0304);
        assert_eq!(bytes, [0xc0, 0x3b, 0x39, 0x98, 0, 0, 0, 1, 1, 2, 3, 4]);
        assert_eq!(Header::parse(&bytes).unwrap().sequence, 0x0102_0304);
    }

    #[test]
    fn feature_matrix_rejects_unsafe_formats() {
        assert_eq!(
            Features::validate(0, FEATURE_INCOMPAT_CSUM_V3 | FEATURE_INCOMPAT_64BIT, 0)
                .unwrap()
                .tag_bytes(),
            16
        );
        assert_eq!(
            Features::validate(0, FEATURE_INCOMPAT_CSUM_V3, 0)
                .unwrap()
                .tag_bytes(),
            16
        );
        for (compat, incompat) in [
            (FEATURE_COMPAT_CHECKSUM, 0),
            (0, FEATURE_INCOMPAT_CSUM_V2),
            (0, FEATURE_INCOMPAT_ASYNC_COMMIT),
            (0, FEATURE_INCOMPAT_FAST_COMMIT),
        ] {
            assert_eq!(
                Features::validate(compat, incompat, 0).unwrap_err().code(),
                ErrCode::ENOTSUP
            );
        }
    }

    #[test]
    fn parses_v3_64bit_tags_and_checks_bounds() {
        let features =
            Features::validate(0, FEATURE_INCOMPAT_CSUM_V3 | FEATURE_INCOMPAT_64BIT, 0).unwrap();
        let mut block = [0; 64];
        header(&mut block, BlockType::Descriptor, 7);
        block[12..16].copy_from_slice(&2u32.to_be_bytes());
        block[16..20].copy_from_slice(&(FLAG_SAME_UUID | FLAG_LAST_TAG).to_be_bytes());
        block[20..24].copy_from_slice(&1u32.to_be_bytes());
        block[24..28].copy_from_slice(&0x1122_3344u32.to_be_bytes());
        let mut tags = DescriptorTags::parse(&block, features, 7, (1u64 << 32) + 3).unwrap();
        assert_eq!(
            tags.next_tag().unwrap().unwrap(),
            DescriptorTag {
                block: (1u64 << 32) + 2,
                flags: FLAG_SAME_UUID | FLAG_LAST_TAG,
                checksum: Some(0x1122_3344),
            }
        );
        assert!(tags.next_tag().unwrap().is_none());
        assert_eq!(
            DescriptorTags::parse(&block, features, 7, 2)
                .unwrap()
                .next_tag()
                .unwrap_err()
                .code(),
            ErrCode::EIO
        );
    }

    #[test]
    fn v3_32bit_tag_rejects_nonzero_high_block_number() {
        let features = Features::validate(0, FEATURE_INCOMPAT_CSUM_V3, 0).unwrap();
        let mut block = [0; 64];
        header(&mut block, BlockType::Descriptor, 7);
        block[12..16].copy_from_slice(&2u32.to_be_bytes());
        block[16..20].copy_from_slice(&(FLAG_SAME_UUID | FLAG_LAST_TAG).to_be_bytes());
        block[20..24].copy_from_slice(&1u32.to_be_bytes());
        let mut tags = DescriptorTags::parse(&block, features, 7, 16).unwrap();
        assert_eq!(tags.next_tag().unwrap_err().code(), ErrCode::EIO);

        block[20..24].fill(0);
        let mut tags = DescriptorTags::parse(&block, features, 7, 16).unwrap();
        assert_eq!(tags.next_tag().unwrap().unwrap().block, 2);
    }

    #[test]
    fn checksum_helpers_have_stable_golden_values() {
        assert_eq!(tag_checksum(u32::MAX, 0x0102_0304, b"jbd2"), 0x7d34_43ff);
        let mut block = [0u8; 32];
        header(&mut block, BlockType::Descriptor, 9);
        assert_eq!(block_checksum(0x1234_5678, &block).unwrap(), 0xa454_bd12);
    }

    #[test]
    fn parses_64bit_revoke_records_safely() {
        let features =
            Features::validate(0, FEATURE_INCOMPAT_REVOKE | FEATURE_INCOMPAT_64BIT, 0).unwrap();
        let mut block = [0; 32];
        header(&mut block, BlockType::Revoke, 3);
        block[12..16].copy_from_slice(&24u32.to_be_bytes());
        block[16..24].copy_from_slice(&0x0000_0001_0000_0002u64.to_be_bytes());
        assert_eq!(
            parse_revoke_records(&block, features, 3, 1u64 << 34).unwrap(),
            [0x0000_0001_0000_0002]
        );
    }

    #[test]
    fn parses_v2_superblock_and_detects_checksum_damage() {
        let mut bytes = [0u8; SUPERBLOCK_BYTES];
        header(&mut bytes, BlockType::SuperblockV2, 11);
        bytes[12..16].copy_from_slice(&4096u32.to_be_bytes());
        bytes[16..20].copy_from_slice(&1024u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&1u32.to_be_bytes());
        bytes[24..28].copy_from_slice(&11u32.to_be_bytes());
        bytes[28..32].copy_from_slice(&1u32.to_be_bytes());
        bytes[40..44]
            .copy_from_slice(&(FEATURE_INCOMPAT_CSUM_V3 | FEATURE_INCOMPAT_64BIT).to_be_bytes());
        bytes[48..64].copy_from_slice(b"0123456789abcdef");
        bytes[80] = CRC32C_CHKSUM;
        let checksum = superblock_checksum(&bytes).unwrap();
        bytes[252..256].copy_from_slice(&checksum.to_be_bytes());
        let sb = Superblock::parse(&bytes, 4096).unwrap();
        assert_eq!(sb.sequence, 11);
        assert_eq!(sb.features.checksum, ChecksumMode::V3);

        bytes[100] ^= 1;
        assert_eq!(
            Superblock::parse(&bytes, 4096).unwrap_err().code(),
            ErrCode::EIO
        );
    }
}
