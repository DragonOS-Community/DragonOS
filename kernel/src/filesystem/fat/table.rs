//! Serialized FAT-table I/O and bounded chain traversal.
//!
//! `fat_cache` guards the whole read/RMW/publication sequence, not just the
//! LRU lookup. Readers cannot republish a value read before a concurrent write.

use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use system_error::SystemError;

use super::io::FatStorage;
use crate::driver::base::block::block_device::LBA_SIZE;

use super::{
    bpb::{BiosParameterBlock, FATType},
    entry::FATEntry,
    fs::{Cluster, FATFileSystem},
};

pub(super) fn read_exact_at(
    disk: &FatStorage,
    buffer: &mut [u8],
    offset: usize,
) -> Result<(), SystemError> {
    // These metadata windows are LBA aligned. Bypass byte-I/O's implicit RMW,
    // whose legacy helper may otherwise hide a short underlying block transfer.
    if !offset.is_multiple_of(LBA_SIZE) || !buffer.len().is_multiple_of(LBA_SIZE) {
        return Err(SystemError::EINVAL);
    }
    if disk.read_at(buffer, offset / LBA_SIZE)? != buffer.len() {
        return Err(SystemError::EIO);
    }
    Ok(())
}

pub(super) fn write_exact_at(
    disk: &FatStorage,
    buffer: &[u8],
    offset: usize,
) -> Result<(), SystemError> {
    if !offset.is_multiple_of(LBA_SIZE) || !buffer.len().is_multiple_of(LBA_SIZE) {
        return Err(SystemError::EINVAL);
    }
    if disk.write_at(buffer, offset / LBA_SIZE)? != buffer.len() {
        return Err(SystemError::EIO);
    }
    Ok(())
}

const SECTOR_CACHE_SLOTS: usize = 32;

struct FatSector {
    offset: Option<u64>,
    bytes: [u8; LBA_SIZE],
    age: u64,
}

/// One write-through metadata cache. Slots are prepared before publication;
/// cache misses, replacement and error invalidation never allocate under NOIO.
pub(super) struct FatSectorCache {
    slots: Vec<FatSector>,
    age: u64,
}

impl core::fmt::Debug for FatSectorCache {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("FatSectorCache")
            .field("slots", &self.slots.len())
            .field(
                "valid",
                &self
                    .slots
                    .iter()
                    .filter(|slot| slot.offset.is_some())
                    .count(),
            )
            .finish()
    }
}

impl FatSectorCache {
    pub(super) fn new() -> Result<Self, SystemError> {
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(SECTOR_CACHE_SLOTS)
            .map_err(|_| SystemError::ENOMEM)?;
        // Initialize one sector at a time, not a 16KiB stack array.
        for _ in 0..SECTOR_CACHE_SLOTS {
            slots.push(FatSector {
                offset: None,
                bytes: [0; LBA_SIZE],
                age: 0,
            });
        }
        Ok(Self { slots, age: 0 })
    }

    fn touch(&mut self, index: usize) {
        if self.age == u64::MAX {
            // Ages only choose a victim; resetting them cannot invalidate data.
            for slot in &mut self.slots {
                slot.age = 0;
            }
            self.age = 0;
        }
        self.age += 1;
        self.slots[index].age = self.age;
    }

    fn replacement(&self) -> usize {
        self.slots
            .iter()
            .position(|slot| slot.offset.is_none())
            .unwrap_or_else(|| {
                self.slots
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, slot)| slot.age)
                    .map(|(index, _)| index)
                    .expect("FAT sector cache is nonempty")
            })
    }

    fn load(&mut self, fs: &FATFileSystem, offset: u64) -> Result<usize, SystemError> {
        if let Some(index) = self
            .slots
            .iter()
            .position(|slot| slot.offset == Some(offset))
        {
            self.touch(index);
            return Ok(index);
        }
        let index = self.replacement();
        // A short/error read may overwrite only part of the old bytes. Never
        // leave their previous key valid while loading directly into a slot.
        self.slots[index].offset = None;
        read_exact_at(
            &fs.storage,
            &mut self.slots[index].bytes,
            usize::try_from(offset).map_err(|_| SystemError::EOVERFLOW)?,
        )?;
        self.slots[index].offset = Some(offset);
        self.touch(index);
        Ok(index)
    }

    fn invalidate(&mut self, offset: u64) {
        if let Some(slot) = self
            .slots
            .iter_mut()
            .find(|slot| slot.offset == Some(offset))
        {
            slot.offset = None;
        }
    }

    fn publish(&mut self, offset: u64, bytes: &[u8]) {
        debug_assert_eq!(bytes.len(), LBA_SIZE);
        let index = self
            .slots
            .iter()
            .position(|slot| slot.offset == Some(offset))
            .unwrap_or_else(|| self.replacement());
        self.slots[index].bytes.copy_from_slice(bytes);
        self.slots[index].offset = Some(offset);
        self.touch(index);
    }

    fn read_window(
        &mut self,
        fs: &FATFileSystem,
        offset: u64,
        bytes: &mut [u8],
    ) -> Result<(), SystemError> {
        debug_assert!(bytes.len().is_multiple_of(LBA_SIZE));
        for (index, sector) in bytes.chunks_exact_mut(LBA_SIZE).enumerate() {
            let key = offset + (index * LBA_SIZE) as u64;
            let slot = self.load(fs, key)?;
            sector.copy_from_slice(&self.slots[slot].bytes);
        }
        Ok(())
    }
}

impl FATFileSystem {
    pub(super) fn validate_table_geometry(bpb: &BiosParameterBlock) -> Result<(), SystemError> {
        if let FATType::FAT32(fat32) = bpb.fat_type {
            if fat32.ext_flags & 0x80 != 0 && (fat32.ext_flags & 0x0f) >= bpb.num_fats as u16 {
                return Err(SystemError::EINVAL);
            }
        }
        Ok(())
    }

    fn entry_width(&self) -> usize {
        match self.bpb.fat_type {
            FATType::FAT12(_) | FATType::FAT16(_) => 2,
            FATType::FAT32(_) => 4,
        }
    }

    fn entry_offset(&self, cluster: u64, copy: u64) -> Result<u64, SystemError> {
        if copy >= self.bpb.num_fats as u64 {
            return Err(SystemError::EIO);
        }
        let relative = match self.bpb.fat_type {
            FATType::FAT12(_) => cluster.checked_add(cluster / 2),
            FATType::FAT16(_) => cluster.checked_mul(2),
            FATType::FAT32(_) => cluster.checked_mul(4),
        }
        .ok_or(SystemError::EOVERFLOW)?;
        let fat_bytes = self
            .fat_size()
            .checked_mul(self.bpb.bytes_per_sector as u64)
            .ok_or(SystemError::EOVERFLOW)?;
        if relative
            .checked_add(self.entry_width() as u64)
            .is_none_or(|end| end > fat_bytes)
        {
            return Err(SystemError::EIO);
        }
        let start = (self.bpb.rsvd_sec_cnt as u64)
            .checked_mul(self.bpb.bytes_per_sector as u64)
            .and_then(|base| {
                copy.checked_mul(fat_bytes)
                    .and_then(|delta| base.checked_add(delta))
            })
            .ok_or(SystemError::EOVERFLOW)?;
        start.checked_add(relative).ok_or(SystemError::EOVERFLOW)
    }

    fn unpack_entry(&self, cluster: u64, bytes: [u8; 4]) -> u32 {
        match self.bpb.fat_type {
            FATType::FAT12(_) => {
                let packed = u16::from_le_bytes([bytes[0], bytes[1]]) as u32;
                if cluster & 1 != 0 {
                    packed >> 4
                } else {
                    packed & 0x0fff
                }
            }
            FATType::FAT16(_) => u16::from_le_bytes([bytes[0], bytes[1]]) as u32,
            FATType::FAT32(_) => u32::from_le_bytes(bytes) & 0x0fff_ffff,
        }
    }

    fn raw_entry_locked(
        &self,
        cache: &mut FatSectorCache,
        cluster: u64,
    ) -> Result<u32, SystemError> {
        let offset = self.entry_offset(cluster, self.active_fat())?;
        let mut bytes = [0u8; 4];
        for (index, byte) in bytes.iter_mut().enumerate().take(self.entry_width()) {
            let position = offset + index as u64;
            let aligned = position / LBA_SIZE as u64 * LBA_SIZE as u64;
            let slot = cache.load(self, aligned)?;
            *byte = cache.slots[slot].bytes[(position - aligned) as usize];
        }
        Ok(self.unpack_entry(cluster, bytes))
    }

    fn decode_entry(&self, cluster: u64, raw: u32) -> FATEntry {
        let (bad, eoc) = match self.bpb.fat_type {
            FATType::FAT12(_) => (0x0ff7, 0x0ff8),
            FATType::FAT16(_) => (0xfff7, 0xfff8),
            FATType::FAT32(_) => (0x0fff_fff7, 0x0fff_fff8),
        };
        if raw == 0 {
            FATEntry::Unused
        } else if raw >= eoc {
            FATEntry::EndOfChain
        } else if raw == 1 || raw == bad {
            FATEntry::Bad
        } else {
            FATEntry::Next(Cluster {
                cluster_num: raw as u64,
                parent_cluster: cluster,
            })
        }
    }

    fn check_data_cluster(&self, cluster: Cluster) -> Result<(), SystemError> {
        if cluster.cluster_num < 2 || cluster.cluster_num > self.max_cluster_number().cluster_num {
            return Err(SystemError::EIO);
        }
        Ok(())
    }

    pub fn get_fat_entry(&self, cluster: Cluster) -> Result<FATEntry, SystemError> {
        self.check_data_cluster(cluster)?;
        let mut cache = self.fat_cache.lock();
        let raw = self.raw_entry_locked(&mut cache, cluster.cluster_num)?;
        Ok(self.decode_entry(cluster.cluster_num, raw))
    }

    pub fn get_fat_entry_raw(&self, cluster: Cluster) -> Result<u64, SystemError> {
        let mut table = self.fat_cache.lock();
        self.raw_entry_locked(&mut table, cluster.cluster_num)
            .map(u64::from)
    }

    /// Inclusive interval; adjacent entries reuse the shared sector cache.
    pub fn get_free_cluster(&self, start: Cluster, end: Cluster) -> Result<Cluster, SystemError> {
        let start = start.cluster_num.max(2);
        let end = end.cluster_num.min(self.max_cluster_number().cluster_num);
        if start > end {
            return Err(SystemError::ENOSPC);
        }
        let mut table = self.fat_cache.lock();
        for cluster in start..=end {
            if self.raw_entry_locked(&mut table, cluster)? == 0 {
                return Ok(Cluster::new(cluster));
            }
        }
        Err(SystemError::ENOSPC)
    }

    /// Data-chain callers hold fat_lock. Reserved 0/1 entries carry volume flags.
    pub(super) fn set_entry(&self, cluster: Cluster, entry: FATEntry) -> Result<(), SystemError> {
        let number = cluster.cluster_num;
        if number >= 2 {
            self.check_data_cluster(cluster)?;
            if let FATEntry::Next(next) = &entry {
                self.check_data_cluster(*next)?;
            }
        }
        let mask = match self.bpb.fat_type {
            FATType::FAT12(_) => 0x0fff,
            FATType::FAT16(_) => 0xffff,
            FATType::FAT32(_) => 0x0fff_ffff,
        };
        let raw = match &entry {
            FATEntry::Unused => 0,
            FATEntry::Bad => mask - 8,
            FATEntry::EndOfChain => mask,
            FATEntry::Next(next) => {
                u32::try_from(next.cluster_num).map_err(|_| SystemError::EIO)? & mask
            }
        };
        let first_copy = if self.mirroring_enabled() {
            0
        } else {
            self.active_fat()
        };
        let last_copy = if self.mirroring_enabled() {
            self.bpb.num_fats as u64
        } else {
            first_copy + 1
        };
        // Only the largest packed FAT12 RMW window lives on this stack, never
        // the whole cache. The preallocated slots handle all cache operations.
        let mut buffer = [0u8; 2 * LBA_SIZE];
        let mut cache = self.fat_cache.lock();
        if number >= 2 {
            let generation = self.fat_chain_generation.load(Ordering::Relaxed);
            self.fat_chain_generation
                .store(generation.saturating_add(1), Ordering::Release);
        }
        let result = (|| {
            for copy in first_copy..last_copy {
                let offset = self.entry_offset(number, copy)?;
                let aligned = offset / LBA_SIZE as u64 * LBA_SIZE as u64;
                let position = (offset - aligned) as usize;
                let length = (position + self.entry_width()).div_ceil(LBA_SIZE) * LBA_SIZE;
                let window = &mut buffer[..length];
                cache.read_window(self, aligned, window)?;
                // Retain the old sector locally before removing its key. A failed
                // device RMW may have committed a prefix, so its old cache bytes
                // must never be visible again after this point.
                for index in 0..length / LBA_SIZE {
                    cache.invalidate(aligned + (index * LBA_SIZE) as u64);
                }
                match self.bpb.fat_type {
                    FATType::FAT12(_) => {
                        let old = u16::from_le_bytes([window[position], window[position + 1]]);
                        let updated = if number & 1 != 0 {
                            (old & 0x000f) | ((raw as u16) << 4)
                        } else {
                            (old & 0xf000) | raw as u16
                        };
                        window[position..position + 2].copy_from_slice(&updated.to_le_bytes());
                    }
                    FATType::FAT16(_) => {
                        window[position..position + 2].copy_from_slice(&(raw as u16).to_le_bytes())
                    }
                    FATType::FAT32(_) => {
                        let old = u32::from_le_bytes(
                            window[position..position + 4]
                                .try_into()
                                .map_err(|_| SystemError::EIO)?,
                        );
                        window[position..position + 4]
                            .copy_from_slice(&((old & 0xf000_0000) | raw).to_le_bytes());
                    }
                }
                write_exact_at(
                    &self.storage,
                    window,
                    usize::try_from(aligned).map_err(|_| SystemError::EOVERFLOW)?,
                )?;
                for (index, bytes) in window.chunks_exact(LBA_SIZE).enumerate() {
                    cache.publish(aligned + (index * LBA_SIZE) as u64, bytes);
                }
            }
            Ok(())
        })();
        if result.is_err() {
            // Invalidate every potentially affected copy/window, including a
            // successful prefix before a later mirror or cross-sector failure.
            // Fresh reads observe the device's actual partial state, not an old
            // successful value or an unpublished modified sector.
            for copy in first_copy..last_copy {
                if let Ok(offset) = self.entry_offset(number, copy) {
                    let aligned = offset / LBA_SIZE as u64 * LBA_SIZE as u64;
                    let position = (offset - aligned) as usize;
                    let sectors = (position + self.entry_width()).div_ceil(LBA_SIZE);
                    for index in 0..sectors {
                        cache.invalidate(aligned + (index * LBA_SIZE) as u64);
                    }
                }
            }
        }
        result
    }

    pub(super) fn walk_cluster(
        &self,
        mut cluster: Cluster,
        n: u64,
    ) -> Result<Cluster, SystemError> {
        let limit = self
            .max_cluster_number()
            .cluster_num
            .checked_sub(1)
            .ok_or(SystemError::EIO)?;
        if n >= limit {
            return Err(SystemError::EIO);
        }
        for index in 0..=n {
            self.check_data_cluster(cluster)?;
            let entry = self.get_fat_entry(cluster)?;
            if matches!(entry, FATEntry::Unused | FATEntry::Bad) {
                return Err(SystemError::EIO);
            }
            if index == n {
                return Ok(cluster);
            }
            cluster = match entry {
                FATEntry::Next(next) => next,
                _ => return Err(SystemError::EIO),
            };
        }
        Err(SystemError::EIO)
    }

    pub fn chain_len_and_tail(&self, mut cluster: Cluster) -> Result<(u64, Cluster), SystemError> {
        let limit = self
            .max_cluster_number()
            .cluster_num
            .checked_sub(1)
            .ok_or(SystemError::EIO)?;
        for count in 1..=limit {
            self.check_data_cluster(cluster)?;
            match self.get_fat_entry(cluster)? {
                FATEntry::EndOfChain => return Ok((count, cluster)),
                FATEntry::Next(next) => cluster = next,
                FATEntry::Unused | FATEntry::Bad => return Err(SystemError::EIO),
            }
        }
        Err(SystemError::EIO)
    }
}
