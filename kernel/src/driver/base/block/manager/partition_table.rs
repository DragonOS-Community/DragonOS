//! Bounded partition-table decoding shared by boot registration and rescans.
//! All offsets exposed to GenDisk are 512-byte LBAs. GPT/MBR disk fields use
//! the queue's logical sectors, which may contain several such LBAs.
use alloc::vec::Vec;
use system_error::SystemError;

use crate::driver::base::block::block_device::{BlockDevice, GeneralBlockRange, LBA_SIZE};

pub(super) const MAX_PARTITIONS: u32 = 255;
// Linux 6.6 efi.c rejects entry arrays above KMALLOC_MAX_SIZE (4 MiB with
// MAX_ORDER=10/PAGE_SHIFT=12). Keep its bounded scan budget while streaming
// the checksum instead of retaining a multi-megabyte allocation.
const MAX_GPT_TABLE_BYTES: u64 = 4 * 1024 * 1024;

pub(super) struct PartitionDescriptor {
    pub index: u32,
    pub range: GeneralBlockRange,
}

struct Reader<'a> {
    device: &'a dyn BlockDevice,
    factor: u64,
    sectors: u64,
    buffer: Vec<u8>,
}

impl<'a> Reader<'a> {
    fn new(device: &'a dyn BlockDevice) -> Result<Self, SystemError> {
        let logical = device.io_limits().logical_block_size as usize;
        if !(LBA_SIZE..=4096).contains(&logical) || !logical.is_power_of_two() {
            return Err(SystemError::EINVAL);
        }
        let factor = (logical / LBA_SIZE) as u64;
        let mut buffer = Vec::new();
        buffer
            .try_reserve_exact(logical)
            .map_err(|_| SystemError::ENOMEM)?;
        buffer.resize(logical, 0);
        Ok(Self {
            device,
            factor,
            sectors: device.disk_range().len() as u64 / factor,
            buffer,
        })
    }

    fn read(&mut self, sector: u64) -> Result<&[u8], SystemError> {
        if sector >= self.sectors {
            return Err(SystemError::EIO);
        }
        let lba = sector
            .checked_mul(self.factor)
            .ok_or(SystemError::EOVERFLOW)?;
        let lba = usize::try_from(lba).map_err(|_| SystemError::EOVERFLOW)?;
        let count = self.factor as usize;
        if self.device.read_at_sync(lba, count, &mut self.buffer)? != self.buffer.len() {
            return Err(SystemError::EIO);
        }
        Ok(&self.buffer)
    }

    fn descriptor(&self, index: u32, start: u64, count: u64) -> Option<PartitionDescriptor> {
        descriptor(self.factor, self.sectors, index, start, count)
    }
}

fn descriptor(
    factor: u64,
    sectors: u64,
    index: u32,
    start: u64,
    count: u64,
) -> Option<PartitionDescriptor> {
    if index == 0 || index > MAX_PARTITIONS || start >= sectors || count == 0 {
        return None;
    }
    // Linux clips a partition crossing the end of the published device.
    let end = start.saturating_add(count).min(sectors);
    let start = usize::try_from(start.checked_mul(factor)?).ok()?;
    let end = usize::try_from(end.checked_mul(factor)?).ok()?;
    Some(PartitionDescriptor {
        index,
        range: GeneralBlockRange::new(start, end)?,
    })
}

fn le32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}
fn le64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}
fn crc32_update(mut crc: u32, bytes: &[u8]) -> u32 {
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    crc
}

#[derive(Clone, Copy, Default)]
struct MbrEntry {
    kind: u8,
    start: u64,
    count: u64,
}
impl MbrEntry {
    fn extended(self) -> bool {
        matches!(self.kind, 0x05 | 0x0f | 0x85)
    }
}
fn mbr_entries(sector: &[u8]) -> Option<[MbrEntry; 4]> {
    if sector[510..512] != [0x55, 0xaa] {
        return None;
    }
    let mut entries = [MbrEntry::default(); 4];
    for (index, entry) in entries.iter_mut().enumerate() {
        let offset = 446 + index * 16;
        *entry = MbrEntry {
            kind: sector[offset + 4],
            start: u64::from(le32(sector, offset + 8)),
            count: u64::from(le32(sector, offset + 12)),
        };
    }
    Some(entries)
}

fn parse_ebr(
    reader: &mut Reader<'_>,
    first: MbrEntry,
    next_index: &mut u32,
    partitions: &mut Vec<PartitionDescriptor>,
) -> Result<(), SystemError> {
    let mut current = first.start;
    let mut current_size = first.count;
    let mut links_without_data = 0;
    // Match Linux's non-recursive EBR walk: 100 links without data, plus
    // the finite minor namespace for chains which keep producing entries.
    loop {
        links_without_data += 1;
        if *next_index > MAX_PARTITIONS || current >= reader.sectors || links_without_data > 100 {
            break;
        }
        // An unreadable later EBR must not erase already decoded primary or
        // logical partitions, just as Linux parse_extended keeps its prefix.
        let sector = match reader.read(current) {
            Ok(sector) => sector,
            Err(_) => break,
        };
        let Some(entries) = mbr_entries(sector) else {
            break;
        };
        for (slot, entry) in entries.iter().enumerate() {
            if entry.count == 0 || entry.extended() {
                continue;
            }
            let Some(start) = current.checked_add(entry.start) else {
                continue;
            };
            // Linux tolerates the usual first two EBR slots and validates the
            // extra slots against the containing extended range.
            if slot >= 2
                && (entry.start.saturating_add(entry.count) > current_size
                    || start < first.start
                    || start.saturating_add(entry.count) > first.start.saturating_add(first.count))
            {
                continue;
            }
            if let Some(partition) = reader.descriptor(*next_index, start, entry.count) {
                partitions.push(partition);
            }
            *next_index += 1;
            links_without_data = 0;
            if *next_index > MAX_PARTITIONS {
                return Ok(());
            }
        }
        let Some(link) = entries
            .iter()
            .find(|entry| entry.count != 0 && entry.extended())
        else {
            break;
        };
        let Some(next) = first.start.checked_add(link.start) else {
            break;
        };
        current = next;
        current_size = link.count;
    }
    Ok(())
}

fn parse_gpt(
    reader: &mut Reader<'_>,
    header_lba: u64,
) -> Result<(Vec<PartitionDescriptor>, u64), SystemError> {
    let sectors = reader.sectors;
    let factor = reader.factor;
    let header = reader.read(header_lba)?;
    if &header[..8] != b"EFI PART" {
        return Err(SystemError::EINVAL);
    }
    let header_size = le32(header, 12) as usize;
    if !(92..=header.len()).contains(&header_size) {
        return Err(SystemError::EINVAL);
    }
    let original_crc = le32(header, 16);
    let mut crc = crc32_update(!0, &header[..16]);
    crc = crc32_update(crc, &[0; 4]);
    crc = crc32_update(crc, &header[20..header_size]);
    if !crc != original_crc || le64(header, 24) != header_lba {
        return Err(SystemError::EINVAL);
    }
    let alternate_lba = le64(header, 32);
    let first_usable = le64(header, 40);
    let last_usable = le64(header, 48);
    if first_usable > last_usable || last_usable >= sectors || le32(header, 84) != 128 {
        return Err(SystemError::EINVAL);
    }
    // Linux 6.6 accepts exactly the native 128-byte GPT entry layout.
    let table_lba = le64(header, 72);
    let count = u64::from(le32(header, 80));
    let expected_crc = le32(header, 88);
    let bytes = count.checked_mul(128).ok_or(SystemError::EINVAL)?;
    let sector_bytes = reader.buffer.len() as u64;
    let table_sectors = bytes.div_ceil(sector_bytes);
    if count == 0
        || bytes > MAX_GPT_TABLE_BYTES
        || table_lba >= sectors
        || table_lba
            .checked_add(table_sectors)
            .is_none_or(|end| end > sectors)
    {
        return Err(SystemError::EINVAL);
    }
    let mut partitions = Vec::new();
    partitions
        .try_reserve_exact(count.min(u64::from(MAX_PARTITIONS)) as usize)
        .map_err(|_| SystemError::ENOMEM)?;
    let mut crc = !0;
    let mut remaining = bytes;
    let mut index = 1u64;
    for sector in 0..table_sectors {
        let block = reader.read(table_lba + sector)?;
        let length = remaining.min(sector_bytes) as usize;
        crc = crc32_update(crc, &block[..length]);
        // Decode only representable minors, but checksum the whole array.
        for entry in block[..length].chunks_exact(128) {
            if index <= u64::from(MAX_PARTITIONS) && entry[..16].iter().any(|byte| *byte != 0) {
                let start = le64(entry, 32);
                let end = le64(entry, 40);
                if start <= end && end < sectors {
                    if let Some(partition) =
                        descriptor(factor, sectors, index as u32, start, end - start + 1)
                    {
                        partitions.push(partition);
                    }
                }
            }
            index += 1;
        }
        remaining -= length as u64;
    }
    if !crc != expected_crc {
        return Err(SystemError::EINVAL);
    }
    Ok((partitions, alternate_lba))
}

pub(super) fn read_partitions(
    device: &dyn BlockDevice,
) -> Result<Vec<PartitionDescriptor>, SystemError> {
    let mut reader = Reader::new(device)?;
    if reader.sectors == 0 {
        return Ok(Vec::new());
    }
    let sector = reader.read(0)?;
    let Some(entries) = mbr_entries(sector) else {
        return Ok(Vec::new());
    };
    if entries.iter().any(|entry| entry.kind == 0xee) {
        // Match Linux's default (without the force_gpt boot option): a bad
        // primary does NOT authorize trusting a guessed backup at media end.
        // A protective MBR is never exposed as an ordinary data partition.
        if !entries
            .iter()
            .any(|entry| entry.kind == 0xee && entry.start == 1)
        {
            return Ok(Vec::new());
        }
        return match parse_gpt(&mut reader, 1) {
            Ok((partitions, alternate)) => {
                if parse_gpt(&mut reader, alternate).is_err() {
                    log::warn!("Invalid alternate GPT; using the valid primary table");
                }
                Ok(partitions)
            }
            Err(SystemError::ENOMEM) => Err(SystemError::ENOMEM),
            Err(_) => Ok(Vec::new()),
        };
    }
    // EFI is tested before MSDOS in Linux; only the primary DOS decoder
    // rejects a bad boot indicator. Neither parser uses CHS for geometry.
    if (0..4).any(|index| reader.buffer[446 + index * 16] & 0x7f != 0) {
        return Ok(Vec::new());
    }
    let mut partitions = Vec::new();
    partitions
        .try_reserve_exact(MAX_PARTITIONS as usize)
        .map_err(|_| SystemError::ENOMEM)?;
    let mut next_index = 5;
    for (slot, entry) in entries.into_iter().enumerate() {
        if entry.count == 0 {
            continue;
        }
        let count = if entry.extended() {
            entry.count.min(if reader.factor == 1 { 2 } else { 1 })
        } else {
            entry.count
        };
        if let Some(partition) = reader.descriptor(slot as u32 + 1, entry.start, count) {
            partitions.push(partition);
        }
        if entry.extended() {
            parse_ebr(&mut reader, entry, &mut next_index, &mut partitions)?;
        }
    }
    Ok(partitions)
}
