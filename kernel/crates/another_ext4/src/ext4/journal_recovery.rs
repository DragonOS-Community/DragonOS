//! Recovery of an internal JBD2 journal.
//!
//! This module deliberately does not know how ext4 maps the journal inode.
//! Mount code must first build an immutable logical-to-physical map from the
//! journal inode's extent tree and expose it through [`JournalRecoveryIo`].

use crate::jbd2::{
    self, BlockType, ChecksumMode, Header, Superblock, FLAG_DELETED, FLAG_ESCAPE, FLAG_LAST_TAG,
    FLAG_SAME_UUID, MAGIC,
};
use crate::prelude::*;

/// I/O boundary used while the normal filesystem caches are not yet live.
pub(super) trait JournalRecoveryIo {
    fn block_size(&self) -> usize;
    fn filesystem_blocks(&self) -> u64;
    fn read_journal(&self, logical: u32) -> Result<Vec<u8>>;
    fn write_journal(&self, logical: u32, data: &[u8]) -> Result<()>;
    fn write_home(&self, physical: u64, data: &[u8]) -> Result<()>;
    fn is_journal_physical(&self, physical: u64) -> bool;
    fn flush_home(&self) -> Result<()>;
    fn flush_journal(&self) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct RecoveryStats {
    pub transactions: u32,
    pub replayed: u64,
    pub revoke_hits: u64,
}

#[derive(Clone, Copy)]
struct Tag {
    block: u64,
    flags: u32,
    checksum: Option<u32>,
}

#[derive(Clone, Copy)]
enum Pass {
    Scan,
    Revoke,
    Replay,
}

/// Replay every complete transaction and make the journal clean.
///
/// An empty (`s_start == 0`) journal is left untouched. On any error the
/// journal superblock is not changed, so a later mount can retry recovery.
pub(super) fn recover<I: JournalRecoveryIo>(io: &I) -> Result<RecoveryStats> {
    let mut raw_sb = io.read_journal(0)?;
    if raw_sb.len() != io.block_size() {
        return Err(Ext4Error::new(ErrCode::EIO));
    }
    let sb = Superblock::parse(&raw_sb, io.block_size() as u32)?;
    if sb.max_len == 0 {
        return Err(Ext4Error::new(ErrCode::EIO));
    }
    if sb.start == 0 {
        return Ok(RecoveryStats::default());
    }

    let seed = jbd2::checksum_seed(&sb.uuid);
    let end_sequence = walk(
        io,
        &sb,
        seed,
        Pass::Scan,
        None,
        None,
        &mut RecoveryStats::default(),
    )?;
    let mut revokes = BTreeMap::<u64, u32>::new();
    walk(
        io,
        &sb,
        seed,
        Pass::Revoke,
        Some(end_sequence),
        Some(&mut revokes),
        &mut RecoveryStats::default(),
    )?;
    let mut stats = RecoveryStats::default();
    walk(
        io,
        &sb,
        seed,
        Pass::Replay,
        Some(end_sequence),
        Some(&mut revokes),
        &mut stats,
    )?;

    // Linux orders recovered home blocks before invalidating the log. Never
    // advertise a clean journal if a home write or its durability barrier
    // failed.
    io.flush_home()?;
    put_be32(&mut raw_sb, 24, end_sequence.wrapping_add(1))?;
    put_be32(&mut raw_sb, 28, 0)?;
    if sb.features.checksum == ChecksumMode::V3 {
        put_be32(&mut raw_sb, 252, 0)?;
        let checksum = jbd2::superblock_checksum(&raw_sb)?;
        put_be32(&mut raw_sb, 252, checksum)?;
    }
    io.write_journal(0, &raw_sb)?;
    io.flush_journal()?;
    stats.transactions = end_sequence.wrapping_sub(sb.sequence);
    Ok(stats)
}

fn walk<I: JournalRecoveryIo>(
    io: &I,
    sb: &Superblock,
    seed: u32,
    pass: Pass,
    stop: Option<u32>,
    mut revokes: Option<&mut BTreeMap<u64, u32>>,
    stats: &mut RecoveryStats,
) -> Result<u32> {
    let mut pos = sb.start;
    let mut sequence = sb.sequence;
    let ring_len = sb.max_len - sb.first;
    let mut consumed = 0u32;

    loop {
        if stop == Some(sequence) {
            return Ok(sequence);
        }
        let tx_start = pos;
        let mut committed = false;
        let mut transaction_checksum_invalid = false;
        while consumed < ring_len {
            let raw = io.read_journal(pos)?;
            check_size(io, &raw)?;
            let header = match Header::parse(&raw) {
                Ok(h) if h.sequence == sequence => h,
                _ if matches!(pass, Pass::Scan) => return Ok(sequence),
                _ => return Err(Ext4Error::new(ErrCode::EIO)),
            };
            pos = advance(sb, pos);
            consumed += 1;
            match header.block_type {
                BlockType::Descriptor => {
                    if sb.features.checksum == ChecksumMode::V3 {
                        if let Err(error) = jbd2::verify_block_checksum(seed, &raw) {
                            if matches!(pass, Pass::Scan) {
                                transaction_checksum_invalid = true;
                            } else {
                                return Err(error);
                            }
                        }
                    }
                    let tags = parse_tags(&raw, sb, io.filesystem_blocks())?;
                    for tag in tags {
                        if consumed >= ring_len {
                            return if matches!(pass, Pass::Scan) {
                                Ok(sequence)
                            } else {
                                Err(Ext4Error::new(ErrCode::EIO))
                            };
                        }
                        let mut data = io.read_journal(pos)?;
                        check_size(io, &data)?;
                        pos = advance(sb, pos);
                        consumed += 1;
                        if let Some(expected) = tag.checksum {
                            if jbd2::tag_checksum(seed, sequence, &data) != expected {
                                if matches!(pass, Pass::Scan) {
                                    transaction_checksum_invalid = true;
                                } else {
                                    return Err(Ext4Error::new(ErrCode::EIO));
                                }
                            }
                        }
                        if matches!(pass, Pass::Replay) && tag.flags & FLAG_DELETED == 0 {
                            if io.is_journal_physical(tag.block) {
                                return Err(Ext4Error::new(ErrCode::EIO));
                            }
                            let revoked = revokes
                                .as_deref()
                                .and_then(|r| r.get(&tag.block))
                                .map(|&tid| tid_geq(tid, sequence))
                                .unwrap_or(false);
                            if revoked {
                                stats.revoke_hits += 1;
                            } else {
                                if tag.flags & FLAG_ESCAPE != 0 {
                                    data[..4].copy_from_slice(&MAGIC.to_be_bytes());
                                }
                                io.write_home(tag.block, &data)?;
                                stats.replayed += 1;
                            }
                        }
                    }
                }
                BlockType::Revoke => {
                    if sb.features.checksum == ChecksumMode::V3 {
                        if let Err(error) = jbd2::verify_block_checksum(seed, &raw) {
                            if matches!(pass, Pass::Scan) {
                                transaction_checksum_invalid = true;
                            } else {
                                return Err(error);
                            }
                        }
                    }
                    if matches!(pass, Pass::Revoke) {
                        for block in jbd2::parse_revoke_records(
                            &raw,
                            sb.features,
                            sequence,
                            io.filesystem_blocks(),
                        )? {
                            if io.is_journal_physical(block) {
                                return Err(Ext4Error::new(ErrCode::EIO));
                            }
                            let table = revokes.as_deref_mut().ok_or_else(eio)?;
                            match table.get(&block) {
                                Some(&old) if tid_geq(old, sequence) => {}
                                _ => {
                                    table.insert(block, sequence);
                                }
                            }
                        }
                    }
                }
                BlockType::Commit => {
                    if let Err(error) = jbd2::CommitHeader::parse(&raw, sb.features, sequence, seed)
                    {
                        if matches!(pass, Pass::Scan) {
                            return Ok(sequence);
                        }
                        return Err(error);
                    }
                    if transaction_checksum_invalid {
                        return Err(Ext4Error::new(ErrCode::EIO));
                    }
                    committed = true;
                    break;
                }
                _ if matches!(pass, Pass::Scan) => return Ok(sequence),
                _ => return Err(Ext4Error::new(ErrCode::EIO)),
            }
        }
        if !committed {
            return if matches!(pass, Pass::Scan) {
                Ok(sequence)
            } else {
                Err(Ext4Error::new(ErrCode::EIO))
            };
        }
        sequence = sequence.wrapping_add(1);
        if pos == tx_start || consumed >= ring_len {
            return Ok(sequence);
        }
    }
}

fn parse_tags(block: &[u8], sb: &Superblock, target_blocks: u64) -> Result<Vec<Tag>> {
    let tail = if sb.features.checksum == ChecksumMode::V3 {
        4
    } else {
        0
    };
    let mut off = 12usize;
    let end = block.len().checked_sub(tail).ok_or_else(eio)?;
    let mut tags = Vec::new();
    loop {
        let size = sb.features.tag_bytes();
        if off.checked_add(size).ok_or_else(eio)? > end {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        let low = be32(block, off)? as u64;
        let (flags, high, checksum) = if sb.features.checksum == ChecksumMode::V3 {
            let encoded_high = be32(block, off + 8)? as u64;
            if !sb.features.has_64bit && encoded_high != 0 {
                return Err(Ext4Error::new(ErrCode::EIO));
            }
            let high = if sb.features.has_64bit {
                encoded_high
            } else {
                0
            };
            (be32(block, off + 4)?, high, Some(be32(block, off + 12)?))
        } else {
            let flags = be16(block, off + 6)? as u32;
            let high = if sb.features.has_64bit {
                be32(block, off + 8)? as u64
            } else {
                0
            };
            (flags, high, None)
        };
        off += size;
        if flags & FLAG_SAME_UUID == 0 {
            let uuid = block.get(off..off + 16).ok_or_else(eio)?;
            if uuid != sb.uuid {
                return Err(Ext4Error::new(ErrCode::EIO));
            }
            off += 16;
        }
        let target = low | (high << 32);
        if target >= target_blocks {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        tags.push(Tag {
            block: target,
            flags,
            checksum,
        });
        if flags & FLAG_LAST_TAG != 0 {
            return Ok(tags);
        }
    }
}

fn advance(sb: &Superblock, pos: u32) -> u32 {
    if pos + 1 == sb.max_len {
        sb.first
    } else {
        pos + 1
    }
}

fn tid_geq(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) >= 0
}

fn check_size<I: JournalRecoveryIo>(io: &I, block: &[u8]) -> Result<()> {
    if block.len() == io.block_size() {
        Ok(())
    } else {
        Err(eio())
    }
}
fn be16(bytes: &[u8], off: usize) -> Result<u16> {
    let s = bytes.get(off..off + 2).ok_or_else(eio)?;
    Ok(u16::from_be_bytes([s[0], s[1]]))
}
fn be32(bytes: &[u8], off: usize) -> Result<u32> {
    let s = bytes.get(off..off + 4).ok_or_else(eio)?;
    Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}
fn put_be32(bytes: &mut [u8], off: usize, value: u32) -> Result<()> {
    bytes
        .get_mut(off..off + 4)
        .ok_or_else(eio)?
        .copy_from_slice(&value.to_be_bytes());
    Ok(())
}
fn eio() -> Ext4Error {
    Ext4Error::new(ErrCode::EIO)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::RefCell;

    struct MemoryIo {
        journal: RefCell<Vec<Vec<u8>>>,
        home: RefCell<BTreeMap<u64, Vec<u8>>>,
        journal_physical: u64,
        events: RefCell<Vec<&'static str>>,
    }

    impl JournalRecoveryIo for MemoryIo {
        fn block_size(&self) -> usize {
            1024
        }
        fn filesystem_blocks(&self) -> u64 {
            1024
        }
        fn read_journal(&self, logical: u32) -> Result<Vec<u8>> {
            self.journal
                .borrow()
                .get(logical as usize)
                .cloned()
                .ok_or_else(eio)
        }
        fn write_journal(&self, logical: u32, data: &[u8]) -> Result<()> {
            self.events.borrow_mut().push("journal-write");
            self.journal.borrow_mut()[logical as usize].copy_from_slice(data);
            Ok(())
        }
        fn write_home(&self, physical: u64, data: &[u8]) -> Result<()> {
            self.events.borrow_mut().push("home-write");
            self.home.borrow_mut().insert(physical, data.to_vec());
            Ok(())
        }
        fn is_journal_physical(&self, physical: u64) -> bool {
            physical == self.journal_physical
        }
        fn flush_home(&self) -> Result<()> {
            self.events.borrow_mut().push("home-flush");
            Ok(())
        }
        fn flush_journal(&self) -> Result<()> {
            self.events.borrow_mut().push("journal-flush");
            Ok(())
        }
    }

    fn put(block: &mut [u8], off: usize, value: u32) {
        block[off..off + 4].copy_from_slice(&value.to_be_bytes());
    }
    fn hdr(block: &mut [u8], kind: BlockType, seq: u32) {
        Header {
            block_type: kind,
            sequence: seq,
        }
        .encode_into(block)
        .unwrap();
    }
    fn superblock(len: u32, start: u32, seq: u32) -> Vec<u8> {
        let mut b = vec![0; 1024];
        hdr(&mut b, BlockType::SuperblockV2, 0);
        put(&mut b, 12, 1024);
        put(&mut b, 16, len);
        put(&mut b, 20, 1);
        put(&mut b, 24, seq);
        put(&mut b, 28, start);
        b[48..64].copy_from_slice(&[0x5a; 16]);
        b
    }
    fn descriptor(seq: u32, target: u32, escape: bool) -> Vec<u8> {
        let mut b = vec![0; 1024];
        hdr(&mut b, BlockType::Descriptor, seq);
        put(&mut b, 12, target);
        let flags = FLAG_LAST_TAG | if escape { FLAG_ESCAPE } else { 0 };
        b[18..20].copy_from_slice(&(flags as u16).to_be_bytes());
        b[20..36].copy_from_slice(&[0x5a; 16]);
        b
    }
    fn commit(seq: u32) -> Vec<u8> {
        let mut b = vec![0; 1024];
        hdr(&mut b, BlockType::Commit, seq);
        b
    }
    fn revoke(seq: u32, target: u32) -> Vec<u8> {
        let mut b = vec![0; 1024];
        hdr(&mut b, BlockType::Revoke, seq);
        put(&mut b, 12, 20);
        put(&mut b, 16, target);
        b
    }
    fn memory(blocks: Vec<Vec<u8>>) -> MemoryIo {
        MemoryIo {
            journal: RefCell::new(blocks),
            home: RefCell::new(BTreeMap::new()),
            journal_physical: 900,
            events: RefCell::new(Vec::new()),
        }
    }

    #[test]
    fn replays_committed_transaction_across_ring_wrap_and_orders_flushes() {
        let mut blocks = vec![vec![0; 1024]; 8];
        blocks[0] = superblock(8, 7, 42);
        blocks[7] = descriptor(42, 77, true);
        blocks[1] = vec![0x33; 1024];
        blocks[2] = commit(42);
        let io = memory(blocks);
        let stats = recover(&io).unwrap();
        assert_eq!(stats.transactions, 1);
        assert_eq!(&io.home.borrow()[&77][..4], &MAGIC.to_be_bytes());
        assert_eq!(
            &*io.events.borrow(),
            &["home-write", "home-flush", "journal-write", "journal-flush"]
        );
        assert_eq!(be32(&io.journal.borrow()[0], 28).unwrap(), 0);
    }

    #[test]
    fn later_revoke_filters_earlier_committed_data() {
        let mut blocks = vec![vec![0; 1024]; 9];
        blocks[0] = superblock(9, 1, 10);
        // Enable revoke in the journal superblock.
        put(&mut blocks[0], 40, jbd2::FEATURE_INCOMPAT_REVOKE);
        blocks[1] = descriptor(10, 55, false);
        blocks[2] = vec![0xaa; 1024];
        blocks[3] = commit(10);
        blocks[4] = revoke(11, 55);
        blocks[5] = commit(11);
        let io = memory(blocks);
        let stats = recover(&io).unwrap();
        assert_eq!(stats.transactions, 2);
        assert_eq!(stats.revoke_hits, 1);
        assert!(io.home.borrow().is_empty());
    }

    #[test]
    fn incomplete_transaction_is_not_replayed() {
        let mut blocks = vec![vec![0; 1024]; 6];
        blocks[0] = superblock(6, 1, 7);
        blocks[1] = descriptor(7, 12, false);
        blocks[2] = vec![0xcc; 1024];
        let io = memory(blocks);
        assert_eq!(recover(&io).unwrap().transactions, 0);
        assert!(io.home.borrow().is_empty());
    }

    #[test]
    fn rejects_replay_into_the_internal_journal() {
        let mut blocks = vec![vec![0; 1024]; 6];
        blocks[0] = superblock(6, 1, 3);
        blocks[1] = descriptor(3, 900, false);
        blocks[2] = vec![1; 1024];
        blocks[3] = commit(3);
        let io = memory(blocks);
        assert!(recover(&io).is_err());
        assert!(io.home.borrow().is_empty());
        assert_ne!(be32(&io.journal.borrow()[0], 28).unwrap(), 0);
    }
}
