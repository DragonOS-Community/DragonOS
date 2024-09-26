//! Multiboot v1 library
//!
//! This crate is partitially modified from `https://github.com/gz/rust-multiboot` && asterinas
//!
//! The main structs to interact with are [`Multiboot`] for the Multiboot information
//! passed from the bootloader to the kernel at runtime and [`Header`] for the static
//! information passed from the kernel to the bootloader in the kernel image.
//!
//!
//! # Additional documentation
//!   * https://www.gnu.org/software/grub/manual/multiboot/multiboot.html
//!   * http://git.savannah.gnu.org/cgit/grub.git/tree/doc/multiboot.texi?h=multiboot
//!
//! [`Multiboot`]: information/struct.Multiboot.html
//! [`Header`]: header/struct.Header.html
#![no_std]

use core::ffi::CStr;

pub const MAGIC: u32 = 0x2BADB002;

/// The ‘boot_device’ field.
///
/// Partition numbers always start from zero. Unused partition
/// bytes must be set to 0xFF. For example, if the disk is partitioned
/// using a simple one-level DOS partitioning scheme, then
/// ‘part’ contains the DOS partition number, and ‘part2’ and ‘part3’
/// are both 0xFF. As another example, if a disk is partitioned first into
/// DOS partitions, and then one of those DOS partitions is subdivided
/// into several BSD partitions using BSD's disklabel strategy, then ‘part1’
/// contains the DOS partition number, ‘part2’ contains the BSD sub-partition
/// within that DOS partition, and ‘part3’ is 0xFF.
///
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct BootDevice {
    /// Contains the bios drive number as understood by
    /// the bios INT 0x13 low-level disk interface: e.g. 0x00 for the
    /// first floppy disk or 0x80 for the first hard disk.
    pub drive: u8,
    /// Specifies the top-level partition number.
    pub partition1: u8,
    /// Specifies a sub-partition in the top-level partition
    pub partition2: u8,
    /// Specifies a sub-partition in the 2nd-level partition
    pub partition3: u8,
}

impl BootDevice {
    /// Is partition1 a valid partition?
    pub fn partition1_is_valid(&self) -> bool {
        self.partition1 != 0xff
    }

    /// Is partition2 a valid partition?
    pub fn partition2_is_valid(&self) -> bool {
        self.partition2 != 0xff
    }

    /// Is partition3 a valid partition?
    pub fn partition3_is_valid(&self) -> bool {
        self.partition3 != 0xff
    }
}

impl Default for BootDevice {
    fn default() -> Self {
        Self {
            drive: 0xff,
            partition1: 0xff,
            partition2: 0xff,
            partition3: 0xff,
        }
    }
}

/// Representation of Multiboot Information according to specification.
///
/// Reference: https://www.gnu.org/software/grub/manual/multiboot/multiboot.html#Boot-information-format
///
///```text
///         +-------------------+
/// 0       | flags             |    (required)
///         +-------------------+
/// 4       | mem_lower         |    (present if flags[0] is set)
/// 8       | mem_upper         |    (present if flags[0] is set)
///         +-------------------+
/// 12      | boot_device       |    (present if flags[1] is set)
///         +-------------------+
/// 16      | cmdline           |    (present if flags[2] is set)
///         +-------------------+
/// 20      | mods_count        |    (present if flags[3] is set)
/// 24      | mods_addr         |    (present if flags[3] is set)
///         +-------------------+
/// 28 - 40 | syms              |    (present if flags[4] or
///         |                   |                flags[5] is set)
///         +-------------------+
/// 44      | mmap_length       |    (present if flags[6] is set)
/// 48      | mmap_addr         |    (present if flags[6] is set)
///         +-------------------+
/// 52      | drives_length     |    (present if flags[7] is set)
/// 56      | drives_addr       |    (present if flags[7] is set)
///         +-------------------+
/// 60      | config_table      |    (present if flags[8] is set)
///         +-------------------+
/// 64      | boot_loader_name  |    (present if flags[9] is set)
///         +-------------------+
/// 68      | apm_table         |    (present if flags[10] is set)
///         +-------------------+
/// 72      | vbe_control_info  |    (present if flags[11] is set)
/// 76      | vbe_mode_info     |
/// 80      | vbe_mode          |
/// 82      | vbe_interface_seg |
/// 84      | vbe_interface_off |
/// 86      | vbe_interface_len |
///         +-------------------+
/// 88      | framebuffer_addr  |    (present if flags[12] is set)
/// 96      | framebuffer_pitch |
/// 100     | framebuffer_width |
/// 104     | framebuffer_height|
/// 108     | framebuffer_bpp   |
/// 109     | framebuffer_type  |
/// 110-115 | color_info        |
///         +-------------------+
///```
///
#[allow(dead_code)]
#[derive(Debug, Copy, Clone)]
#[repr(C, packed)]
pub struct MultibootInfo {
    /// Indicate whether the below field exists.
    flags: u32,

    /// Physical memory low.
    mem_lower: u32,
    /// Physical memory high.
    mem_upper: u32,

    /// Indicates which BIOS disk device the boot loader loaded the OS image from.
    boot_device: BootDevice,

    /// Command line passed to kernel.
    cmdline: u32,

    /// Modules count.
    pub mods_count: u32,
    /// The start address of modules list, each module structure format:
    /// ```text
    ///         +-------------------+
    /// 0       | mod_start         |
    /// 4       | mod_end           |
    ///         +-------------------+
    /// 8       | string            |
    ///         +-------------------+
    /// 12      | reserved (0)      |
    ///         +-------------------+
    /// ```
    mods_paddr: u32,

    /// If flags[4] = 1, then the field starting at byte 28 are valid:
    /// ```text
    ///         +-------------------+
    /// 28      | tabsize           |
    /// 32      | strsize           |
    /// 36      | addr              |
    /// 40      | reserved (0)      |
    ///         +-------------------+
    /// ```
    /// These indicate where the symbol table from kernel image can be found.
    ///
    /// If flags[5] = 1, then the field starting at byte 28 are valid:
    /// ```text
    ///         +-------------------+
    /// 28      | num               |
    /// 32      | size              |
    /// 36      | addr              |
    /// 40      | shndx             |
    ///         +-------------------+
    /// ```
    /// These indicate where the section header table from an ELF kernel is,
    /// the size of each entry, number of entries, and the string table used as the index of names.
    symbols: [u8; 16],

    memory_map_len: u32,
    memory_map_paddr: u32,

    drives_length: u32,
    drives_addr: u32,

    config_table: u32,

    /// bootloader name paddr
    pub boot_loader_name: u32,

    apm_table: u32,

    vbe_table: VbeInfo,

    pub framebuffer_table: FramebufferTable,
}

impl MultibootInfo {
    /// If true, then the `mem_upper` and `mem_lower` fields are valid.
    pub const FLAG_MEMORY_BOUNDS: u32 = 1 << 0;
    /// If true, then the `boot_device` field is valid.
    pub const FLAG_BOOT_DEVICE: u32 = 1 << 1;
    /// If true, then the `cmdline` field is valid.
    pub const FLAG_CMDLINE: u32 = 1 << 2;
    /// If true, then the `mods_count` and `mods_addr` fields are valid.
    pub const FLAG_MODULES: u32 = 1 << 3;
    /// If true, then the `symbols` field is valid.
    pub const FLAG_SYMBOLS: u32 = 1 << 4;

    pub unsafe fn memory_map(&self, ops: &'static dyn MultibootOps) -> MemoryEntryIter {
        let mmap_addr = ops.phys_2_virt(self.memory_map_paddr as usize);
        let mmap_len = self.memory_map_len as usize;
        MemoryEntryIter {
            cur_ptr: mmap_addr,
            region_end_vaddr: mmap_addr + mmap_len,
        }
    }

    pub unsafe fn modules(&self, ops: &'static dyn MultibootOps) -> Option<ModulesIter> {
        if !self.has_modules() {
            return None;
        }

        let mods_addr = ops.phys_2_virt(self.mods_paddr as usize);
        let end = mods_addr + (self.mods_count as usize) * core::mem::size_of::<MBModule>();
        Some(ModulesIter {
            cur_ptr: mods_addr,
            region_end_vaddr: end,
        })
    }

    pub unsafe fn cmdline(&self, ops: &'static dyn MultibootOps) -> Option<&str> {
        if !self.has_cmdline() {
            return None;
        }

        let cmdline_vaddr = ops.phys_2_virt(self.cmdline as usize);

        let cstr = CStr::from_ptr(cmdline_vaddr as *const i8);
        cstr.to_str().ok()
    }

    #[inline]
    pub fn has_memory_bounds(&self) -> bool {
        self.flags & Self::FLAG_MEMORY_BOUNDS != 0
    }

    #[inline]
    pub fn has_boot_device(&self) -> bool {
        self.flags & Self::FLAG_BOOT_DEVICE != 0
    }

    #[inline]
    pub fn has_cmdline(&self) -> bool {
        self.flags & Self::FLAG_CMDLINE != 0
    }

    #[inline]
    pub fn has_modules(&self) -> bool {
        self.flags & Self::FLAG_MODULES != 0
    }

    #[inline]
    pub fn has_symbols(&self) -> bool {
        self.flags & Self::FLAG_SYMBOLS != 0
    }
}

pub trait MultibootOps {
    fn phys_2_virt(&self, paddr: usize) -> usize;
}

#[derive(Debug, Copy, Clone)]
#[repr(C, packed)]
pub struct VbeInfo {
    pub control_info: u32,
    pub mode_info: u32,
    pub mode: u16,
    pub interface_seg: u16,
    pub interface_off: u16,
    pub interface_len: u16,
}

#[derive(Debug, Copy, Clone)]
#[repr(C, packed)]
pub struct FramebufferTable {
    pub paddr: u64,
    pub pitch: u32,
    pub width: u32,
    pub height: u32,
    pub bpp: u8,
    pub typ: u8,
    color_info: ColorInfo,
}

impl FramebufferTable {
    /// Get the color info from this table.
    pub fn color_info(&self) -> Option<ColorInfoType> {
        unsafe {
            match self.typ {
                0 => Some(ColorInfoType::Palette(self.color_info.palette)),
                1 => Some(ColorInfoType::Rgb(self.color_info.rgb)),
                2 => Some(ColorInfoType::Text),
                _ => None,
            }
        }
    }
}

/// Safe wrapper for `ColorInfo`
#[derive(Debug)]
pub enum ColorInfoType {
    Palette(ColorInfoPalette),
    Rgb(ColorInfoRgb),
    Text,
}

/// Multiboot format for the frambuffer color info
///
/// According to the spec, if type == 0, it's indexed color and
///<rawtext>
///         +----------------------------------+
/// 110     | framebuffer_palette_addr         |
/// 114     | framebuffer_palette_num_colors   |
///         +----------------------------------+
///</rawtext>
/// The address points to an array of `ColorDescriptor`s.
/// If type == 1, it's RGB and
///<rawtext>
///        +----------------------------------+
///110     | framebuffer_red_field_position   |
///111     | framebuffer_red_mask_size        |
///112     | framebuffer_green_field_position |
///113     | framebuffer_green_mask_size      |
///114     | framebuffer_blue_field_position  |
///115     | framebuffer_blue_mask_size       |
///        +----------------------------------+
///</rawtext>
/// (If type == 2, it's just text.)
#[repr(C)]
#[derive(Clone, Copy)]
union ColorInfo {
    palette: ColorInfoPalette,
    rgb: ColorInfoRgb,
    _union_align: [u32; 2usize],
}

impl core::fmt::Debug for ColorInfo {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        unsafe {
            f.debug_struct("ColorInfo")
                .field("palette", &self.palette)
                .field("rgb", &self.rgb)
                .finish()
        }
    }
}

// default type is 0, so indexed color
impl Default for ColorInfo {
    fn default() -> Self {
        Self {
            palette: ColorInfoPalette {
                palette_addr: 0,
                palette_num_colors: 0,
            },
        }
    }
}

/// Information for indexed color mode
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ColorInfoPalette {
    palette_addr: u32,
    palette_num_colors: u16,
}

/// Information for direct RGB color mode
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ColorInfoRgb {
    pub red_field_position: u8,
    pub red_mask_size: u8,
    pub green_field_position: u8,
    pub green_mask_size: u8,
    pub blue_field_position: u8,
    pub blue_mask_size: u8,
}

/// Types that define if the memory is usable or not.
#[derive(Debug, PartialEq, Eq)]
pub enum MemoryType {
    /// memory, available to OS
    Available = 1,
    /// reserved, not available (rom, mem map dev)
    Reserved = 2,
    /// ACPI Reclaim Memory
    ACPI = 3,
    /// ACPI NVS Memory
    NVS = 4,
    /// defective RAM modules
    Defect = 5,
}

/// A memory entry in the memory map header info region.
///
/// The memory layout of the entry structure doesn't fit in any scheme
/// provided by Rust:
///
/// ```text
///         +-------------------+   <- start of the struct pointer
/// -4      | size              |
///         +-------------------+
/// 0       | base_addr         |
/// 8       | length            |
/// 16      | type              |
///         +-------------------+
/// ```
///
/// The start of a entry is not 64-bit aligned. Although the boot
/// protocol may provide the `mmap_addr` 64-bit aligned when added with
/// 4, it is not guaranteed. So we need to use pointer arithmetic to
/// access the fields.
pub struct MemoryEntry {
    ptr: usize,
}

impl MemoryEntry {
    pub fn size(&self) -> u32 {
        // SAFETY: the entry can only be contructed from a valid address.
        unsafe { (self.ptr as *const u32).read_unaligned() }
    }

    pub fn base_addr(&self) -> u64 {
        // SAFETY: the entry can only be contructed from a valid address.
        unsafe { ((self.ptr + 4) as *const u64).read_unaligned() }
    }

    pub fn length(&self) -> u64 {
        // SAFETY: the entry can only be contructed from a valid address.
        unsafe { ((self.ptr + 12) as *const u64).read_unaligned() }
    }

    pub fn memory_type(&self) -> MemoryType {
        let typ_val = unsafe { ((self.ptr + 20) as *const u8).read_unaligned() };
        // The meaning of the values are however documented clearly by the manual.
        match typ_val {
            1 => MemoryType::Available,
            2 => MemoryType::Reserved,
            3 => MemoryType::ACPI,
            4 => MemoryType::NVS,
            5 => MemoryType::Defect,
            _ => MemoryType::Reserved,
        }
    }
}

/// A memory entry iterator in the memory map header info region.
#[derive(Debug, Copy, Clone)]
pub struct MemoryEntryIter {
    cur_ptr: usize,
    region_end_vaddr: usize,
}

impl Iterator for MemoryEntryIter {
    type Item = MemoryEntry;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cur_ptr >= self.region_end_vaddr {
            return None;
        }
        let entry = MemoryEntry { ptr: self.cur_ptr };
        self.cur_ptr += entry.size() as usize + 4;
        Some(entry)
    }
}

/// Multiboot format to information about module
#[repr(C)]
pub struct MBModule {
    /// Start address of module in memory.
    start: u32,

    /// End address of module in memory.
    end: u32,

    /// The `string` field provides an arbitrary string to be associated
    /// with that particular boot module.
    ///
    /// It is a zero-terminated ASCII string, just like the kernel command line.
    /// The `string` field may be 0 if there is no string associated with the module.
    /// Typically the string might be a command line (e.g. if the operating system
    /// treats boot modules as executable programs), or a pathname
    /// (e.g. if the operating system treats boot modules as files in a file system),
    /// but its exact use is specific to the operating system.
    string: u32,

    /// Must be zero.
    reserved: u32,
}

impl MBModule {
    #[inline]
    pub fn start(&self) -> u32 {
        self.start
    }

    #[inline]
    pub fn end(&self) -> u32 {
        self.end
    }

    pub fn string(&self) -> u32 {
        self.string
    }

    pub fn reserved(&self) -> u32 {
        self.reserved
    }
}

impl core::fmt::Debug for MBModule {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        write!(
            f,
            "MBModule {{ start: {}, end: {}, string: {}, reserved: {} }}",
            self.start, self.end, self.string, self.reserved
        )
    }
}

#[derive(Debug, Copy, Clone)]
pub struct ModulesIter {
    cur_ptr: usize,
    region_end_vaddr: usize,
}

impl Iterator for ModulesIter {
    type Item = MBModule;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cur_ptr >= self.region_end_vaddr {
            return None;
        }
        let mb_module = unsafe { (self.cur_ptr as *const MBModule).read() };

        self.cur_ptr += core::mem::size_of::<MBModule>();
        Some(mb_module)
    }
}
