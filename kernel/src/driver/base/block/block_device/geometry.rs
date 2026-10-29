/// Block queue limits, in bytes except for the explicit sector argument.
///
/// These describe the device queue rather than the VFS soft block size. A
/// backend may return an updated snapshot when reconfiguring its queue.
#[derive(Debug, Clone, Copy)]
pub struct BlockDeviceIoLimits {
    pub logical_block_size: u32,
    pub physical_block_size: u32,
    pub io_min: u32,
    pub io_opt: u32,
    pub alignment_offset: u32,
    pub misaligned: bool,
}

impl BlockDeviceIoLimits {
    /// Baseline queue limits; this does not probe hardware topology.
    pub const fn new(logical_block_size: u32) -> Self {
        Self {
            logical_block_size,
            physical_block_size: logical_block_size,
            io_min: logical_block_size,
            io_opt: 0,
            alignment_offset: 0,
            misaligned: false,
        }
    }

    pub fn is_valid(&self) -> bool {
        self.logical_block_size >= 512
            && self.logical_block_size <= i32::MAX as u32
            && self.logical_block_size.is_power_of_two()
            && self.physical_block_size >= self.logical_block_size
            && self.physical_block_size.is_power_of_two()
            && self.io_min >= self.physical_block_size
            && self.io_min.is_multiple_of(self.logical_block_size)
            && (self.io_opt == 0 || self.io_opt.is_multiple_of(self.logical_block_size))
            && self.alignment_offset < self.physical_block_size.max(self.io_min)
    }

    /// Linux bdev_alignment_offset(): partitions compensate for their start
    /// in units of 512-byte sectors, using max(physical, minimum I/O).
    pub fn node_alignment(&self, start_sector: u64, partition: bool) -> Option<i32> {
        if !self.is_valid() {
            return None;
        }
        if self.misaligned {
            return Some(-1);
        }
        let offset = if partition {
            let granularity = u64::from(self.physical_block_size.max(self.io_min));
            // Reduce the LBA before multiplying, even for a very large disk.
            let displacement = (start_sector % (granularity / 512)) * 512;
            (granularity + u64::from(self.alignment_offset) - displacement) % granularity
        } else {
            u64::from(self.alignment_offset)
        };
        i32::try_from(offset).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::BlockDeviceIoLimits;

    #[test]
    fn partition_alignment_uses_minimum_io_and_device_offset() {
        let limits = BlockDeviceIoLimits {
            logical_block_size: 512,
            physical_block_size: 4096,
            io_min: 8192,
            io_opt: 65536,
            alignment_offset: 1024,
            misaligned: false,
        };
        assert_eq!(limits.node_alignment(0, false), Some(1024));
        assert_eq!(limits.node_alignment(1, true), Some(512));
        assert_eq!(limits.node_alignment(3, true), Some(7680));
        assert_eq!(limits.node_alignment(u64::MAX, true), Some(1536));
    }

    #[test]
    fn non_power_of_two_minimum_io_uses_modulo() {
        let mut limits = BlockDeviceIoLimits::new(4096);
        limits.io_min = 12288;
        assert_eq!(limits.node_alignment(8, true), Some(8192));
        assert_eq!(limits.node_alignment(24, true), Some(0));
    }

    #[test]
    fn misaligned_and_invalid_geometry_are_distinct() {
        let mut limits = BlockDeviceIoLimits::new(4096);
        limits.misaligned = true;
        assert_eq!(limits.node_alignment(8, true), Some(-1));
        limits.io_min = 0;
        assert!(!limits.is_valid());
        assert_eq!(limits.node_alignment(8, true), None);
        assert!(!BlockDeviceIoLimits::new(0).is_valid());
        assert!(!BlockDeviceIoLimits::new(768).is_valid());
        assert!(!BlockDeviceIoLimits::new(1 << 31).is_valid());
    }
}
