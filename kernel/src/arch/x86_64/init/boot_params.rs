use core::ffi::{c_uchar, c_uint, c_ulonglong, c_ushort};

#[repr(C, packed)]
pub struct ScreenInfo {
    pub orig_x: c_uchar,             /* 0x00 */
    pub orig_y: c_uchar,             /* 0x01 */
    pub ext_mem_k: c_ushort,         /* 0x02 */
    pub orig_video_page: c_ushort,   /* 0x04 */
    pub orig_video_mode: c_uchar,    /* 0x06 */
    pub orig_video_cols: c_uchar,    /* 0x07 */
    pub flags: c_uchar,              /* 0x08 */
    pub unused2: c_uchar,            /* 0x09 */
    pub orig_video_ega_bx: c_ushort, /* 0x0a */
    pub unused3: c_ushort,           /* 0x0c */
    pub orig_video_lines: c_uchar,   /* 0x0e */
    pub orig_video_is_vga: c_uchar,  /* 0x0f */
    pub orig_video_points: c_ushort, /* 0x10 */

    /* VESA graphic mode -- linear frame buffer */
    pub lfb_width: c_ushort,       /* 0x12 */
    pub lfb_height: c_ushort,      /* 0x14 */
    pub lfb_depth: c_ushort,       /* 0x16 */
    pub lfb_base: c_uint,          /* 0x18 */
    pub lfb_size: c_uint,          /* 0x1c */
    pub cl_magic: c_ushort,        /* 0x20 */
    pub cl_offset: c_ushort,       /* 0x20 + 2 */
    pub lfb_linelength: c_ushort,  /* 0x24 */
    pub red_size: c_uchar,         /* 0x26 */
    pub red_pos: c_uchar,          /* 0x27 */
    pub green_size: c_uchar,       /* 0x28 */
    pub green_pos: c_uchar,        /* 0x29 */
    pub blue_size: c_uchar,        /* 0x2a */
    pub blue_pos: c_uchar,         /* 0x2b */
    pub rsvd_size: c_uchar,        /* 0x2c */
    pub rsvd_pos: c_uchar,         /* 0x2d */
    pub vesapm_seg: c_ushort,      /* 0x2e */
    pub vesapm_off: c_ushort,      /* 0x30 */
    pub pages: c_ushort,           /* 0x32 */
    pub vesa_attributes: c_ushort, /* 0x34 */
    pub capabilities: c_uint,      /* 0x36 */
    pub ext_lfb_base: c_uint,      /* 0x3a */
    pub _reserved: [c_uchar; 2],   /* 0x3e */
}

#[repr(C, packed)]
pub struct ApmBiosInfo {
    pub version: c_ushort,     /* 0x00 */
    pub cseg: c_ushort,        /* 0x02 */
    pub offset: c_uint,        /* 0x04 */
    pub cseg_16: c_ushort,     /* 0x08 */
    pub dseg: c_ushort,        /* 0x0a */
    pub flags: c_ushort,       /* 0x0c */
    pub cseg_len: c_ushort,    /* 0x0e */
    pub cseg_16_len: c_ushort, /* 0x10 */
    pub dseg_len: c_ushort,    /* 0x12 */
}

#[repr(C, packed)]
pub struct IstInfo {
    pub signature: c_uint,  /* 0x00 */
    pub command: c_uint,    /* 0x04 */
    pub event: c_uint,      /* 0x08 */
    pub perf_level: c_uint, /* 0x0c */
}

#[repr(C, packed)]
pub struct SysDescTable {
    pub length: c_ushort,     /* 0x00 */
    pub table: [c_uchar; 14], /* 0x02 */
}

#[repr(C, packed)]
pub struct OlpcOfwHeader {
    pub ofw_magic: c_uint,      /* OFW signature - 0x00 */
    pub ofw_version: c_uint,    /* 0x04 */
    pub cif_handler: c_uint,    /* callback into OFW - 0x08 */
    pub irq_desc_table: c_uint, /* 0x0c */
}

#[repr(C, packed)]
pub struct EdidInfo {
    pub dummy: [u8; 128],
}

#[repr(C, packed)]
pub struct EfiInfo {
    pub efi_loader_signature: c_uint, /* 0x00 */
    pub efi_systab: c_uint,           /* 0x04 */
    pub efi_memdesc_size: c_uint,     /* 0x08 */
    pub efi_memdesc_version: c_uint,  /* 0x0c */
    pub efi_memmap: c_uint,           /* 0x10 */
    pub efi_memmap_size: c_uint,      /* 0x14 */
    pub efi_systab_hi: c_uint,        /* 0x18 */
    pub efi_memmap_hi: c_uint,        /* 0x1c */
}

#[repr(C, packed)]
pub struct SetupHeader {
    pub setup_sects: c_uchar,               /* 0x00 */
    pub root_flags: c_ushort,               /* 0x01 */
    pub syssize: c_uint,                    /* 0x03 */
    pub ram_size: c_ushort,                 /* 0x07 */
    pub vid_mode: c_ushort,                 /* 0x09 */
    pub root_dev: c_ushort,                 /* 0x0b */
    pub boot_flag: c_ushort,                /* 0x0d */
    pub jump: c_ushort,                     /* 0x0f */
    pub header: c_uint,                     /* 0x11 */
    pub version: c_ushort,                  /* 0x15 */
    pub realmode_swtch: c_uint,             /* 0x17 */
    pub start_sys_seg: c_ushort,            /* 0x1b */
    pub kernel_version: c_ushort,           /* 0x1d */
    pub type_of_loader: c_uchar,            /* 0x1f */
    pub loadflags: c_uchar,                 /* 0x20 */
    pub setup_move_size: c_ushort,          /* 0x21 */
    pub code32_start: c_uint,               /* 0x23 */
    pub ramdisk_image: c_uint,              /* 0x27 */
    pub ramdisk_size: c_uint,               /* 0x2b */
    pub bootsect_kludge: c_uint,            /* 0x2f */
    pub heap_end_ptr: c_ushort,             /* 0x33 */
    pub ext_loader_ver: c_uchar,            /* 0x35 */
    pub ext_loader_type: c_uchar,           /* 0x36 */
    pub cmd_line_ptr: c_uint,               /* 0x37 */
    pub initrd_addr_max: c_uint,            /* 0x3b */
    pub kernel_alignment: c_uint,           /* 0x3f */
    pub relocatable_kernel: c_uchar,        /* 0x43 */
    pub min_alignment: c_uchar,             /* 0x44 */
    pub xloadflags: c_ushort,               /* 0x45 */
    pub cmdline_size: c_uint,               /* 0x47 */
    pub hardware_subarch: c_uint,           /* 0x4b */
    pub hardware_subarch_data: c_ulonglong, /* 0x4f */
    pub payload_offset: c_uint,             /* 0x57 */
    pub payload_length: c_uint,             /* 0x5b */
    pub setup_data: c_ulonglong,            /* 0x5f */
    pub pref_address: c_ulonglong,          /* 0x67 */
    pub init_size: c_uint,                  /* 0x6f */
    pub handover_offset: c_uint,            /* 0x73 */
    pub kernel_info_offset: c_uint,         /* 0x77 */
}

#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct BootE820Entry {
    pub addr: u64,  /* 0x00 */
    pub size: u64,  /* 0x08 */
    pub type_: u32, /* 0x10 */
}

#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct EddDeviceParams {
    pub length: c_ushort,                 /* 0x00 */
    pub info_flags: c_ushort,             /* 0x02 */
    pub num_default_cylinders: c_uint,    /* 0x04 */
    pub num_default_heads: c_uint,        /* 0x08 */
    pub sectors_per_track: c_uint,        /* 0x0c */
    pub number_of_sectors: c_ulonglong,   /* 0x10 */
    pub bytes_per_sector: c_ushort,       /* 0x18 */
    pub dpte_ptr: c_uint,                 /* 0x1a */
    pub key: c_ushort,                    /* 0x1e */
    pub device_path_info_length: c_uchar, /* 0x20 */
    pub reserved2: c_uchar,               /* 0x21 */
    pub reserved3: c_ushort,              /* 0x22 */
    pub host_bus_type: [c_uchar; 4],      /* 0x24 */
    pub interface_type: [c_uchar; 8],     /* 0x28 */
    pub interface_path: EddInterfacePath, /* 0x30 */
    pub device_path: EddDevicePath,       /* 0x38 */
    pub reserved4: c_uchar,               /* 0x48 */
    pub checksum: c_uchar,                /* 0x49 */
}

#[repr(C, packed)]
#[derive(Copy, Clone)]
pub union EddInterfacePath {
    pub isa: EddIsaPath,
    pub pci: EddPciPath,
    pub ibnd: EddIbndPath,
    pub xprs: EddXprsPath,
    pub htpt: EddHtptPath,
    pub unknown: EddUnknownPath,
}

#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct EddIsaPath {
    pub base_address: c_ushort, /* 0x00 */
    pub reserved1: c_ushort,    /* 0x02 */
    pub reserved2: c_uint,      /* 0x04 */
}

#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct EddPciPath {
    pub bus: c_uchar,      /* 0x00 */
    pub slot: c_uchar,     /* 0x01 */
    pub function: c_uchar, /* 0x02 */
    pub channel: c_uchar,  /* 0x03 */
    pub reserved: c_uint,  /* 0x04 */
}

#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct EddIbndPath {
    pub reserved: c_ulonglong, /* 0x00 */
}

#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct EddXprsPath {
    pub reserved: c_ulonglong, /* 0x00 */
}

#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct EddHtptPath {
    pub reserved: c_ulonglong, /* 0x00 */
}

#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct EddUnknownPath {
    pub reserved: c_ulonglong, /* 0x00 */
}

#[repr(C, packed)]
#[derive(Copy, Clone)]
pub union EddDevicePath {
    pub ata: EddAtaPath,
    pub atapi: EddAtapiPath,
    pub scsi: EddScsiPath,
    pub usb: EddUsbPath,
    pub i1394: EddI1394Path,
    pub fibre: EddFibrePath,
    pub i2o: EddI2oPath,
    pub raid: EddRaidPath,
    pub sata: EddSataPath,
    pub unknown: EddUnknownDevicePath,
}

#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct EddAtaPath {
    pub device: c_uchar,        /* 0x00 */
    pub reserved1: c_uchar,     /* 0x01 */
    pub reserved2: c_ushort,    /* 0x02 */
    pub reserved3: c_uint,      /* 0x04 */
    pub reserved4: c_ulonglong, /* 0x08 */
}

#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct EddAtapiPath {
    pub device: c_uchar,        /* 0x00 */
    pub lun: c_uchar,           /* 0x01 */
    pub reserved1: c_uchar,     /* 0x02 */
    pub reserved2: c_uchar,     /* 0x03 */
    pub reserved3: c_uint,      /* 0x04 */
    pub reserved4: c_ulonglong, /* 0x08 */
}

#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct EddScsiPath {
    pub id: c_ushort,        /* 0x00 */
    pub lun: c_ulonglong,    /* 0x02 */
    pub reserved1: c_ushort, /* 0x0a */
    pub reserved2: c_uint,   /* 0x0c */
}

#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct EddUsbPath {
    pub serial_number: c_ulonglong, /* 0x00 */
    pub reserved: c_ulonglong,      /* 0x08 */
}

#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct EddI1394Path {
    pub eui: c_ulonglong,      /* 0x00 */
    pub reserved: c_ulonglong, /* 0x08 */
}

#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct EddFibrePath {
    pub wwid: c_ulonglong, /* 0x00 */
    pub lun: c_ulonglong,  /* 0x08 */
}

#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct EddI2oPath {
    pub identity_tag: c_ulonglong, /* 0x00 */
    pub reserved: c_ulonglong,     /* 0x08 */
}

#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct EddRaidPath {
    pub array_number: c_uint,   /* 0x00 */
    pub reserved1: c_uint,      /* 0x04 */
    pub reserved2: c_ulonglong, /* 0x08 */
}

#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct EddSataPath {
    pub device: c_uchar,        /* 0x00 */
    pub reserved1: c_uchar,     /* 0x01 */
    pub reserved2: c_ushort,    /* 0x02 */
    pub reserved3: c_uint,      /* 0x04 */
    pub reserved4: c_ulonglong, /* 0x08 */
}

#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct EddUnknownDevicePath {
    pub reserved1: c_ulonglong, /* 0x00 */
    pub reserved2: c_ulonglong, /* 0x08 */
}

#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct EddInfo {
    pub device: c_uchar,                   /* 0x00 */
    pub version: c_uchar,                  /* 0x01 */
    pub interface_support: c_ushort,       /* 0x02 */
    pub legacy_max_cylinder: c_ushort,     /* 0x04 */
    pub legacy_max_head: c_uchar,          /* 0x06 */
    pub legacy_sectors_per_track: c_uchar, /* 0x07 */
    pub params: EddDeviceParams,           /* 0x08 */
}

/// 对齐 Linux 的 boot_params
/// https://code.dragonos.org.cn/xref/linux-6.1.9/arch/x86/include/uapi/asm/bootparam.h#185
#[repr(C, packed)]
pub struct ArchBootParams {
    pub screen_info: ScreenInfo,     /* 0x000 */
    pub apm_bios_info: ApmBiosInfo,  /* 0x040 */
    pub _pad2: [c_uchar; 4],         /* 0x054 */
    pub tboot_addr: c_ulonglong,     /* 0x058 */
    pub ist_info: IstInfo,           /* 0x060 */
    pub acpi_rsdp_addr: c_ulonglong, /* 0x070 */
    pub _pad3: [c_uchar; 8],         /* 0x078 */
    pub hd0_info: [c_uchar; 16],     /* obsolete! */
    /* 0x080 */
    pub hd1_info: [c_uchar; 16], /* obsolete! */
    /* 0x090 */
    pub sys_desc_table: SysDescTable, /* obsolete! */
    /* 0x0a0 */
    pub olpc_ofw_header: OlpcOfwHeader,   /* 0x0b0 */
    pub ext_ramdisk_image: c_uint,        /* 0x0c0 */
    pub ext_ramdisk_size: c_uint,         /* 0x0c4 */
    pub ext_cmd_line_ptr: c_uint,         /* 0x0c8 */
    pub _pad4: [c_uchar; 112],            /* 0x0cc */
    pub cc_blob_address: c_uint,          /* 0x13c */
    pub edid_info: EdidInfo,              /* 0x140 */
    pub efi_info: EfiInfo,                /* 0x1c0 */
    pub alt_mem_k: c_uint,                /* 0x1e0 */
    pub scratch: c_uint,                  /* 0x1e4 */
    pub e820_entries: c_uchar,            /* 0x1e8 */
    pub eddbuf_entries: c_uchar,          /* 0x1e9 */
    pub edd_mbr_sig_buf_entries: c_uchar, /* 0x1ea */
    pub kbd_status: c_uchar,              /* 0x1eb */
    pub secure_boot: c_uchar,             /* 0x1ec */
    pub _pad5: [c_uchar; 2],              /* 0x1ed */
    pub sentinel: c_uchar,                /* 0x1ef */
    pub _pad6: [c_uchar; 1],              /* 0x1f0 */
    pub hdr: SetupHeader,                 /* 0x1f1 */
    pub _pad7: [c_uchar; 0x290 - 0x1f1 - core::mem::size_of::<SetupHeader>()], /* 0x290 - 0x1f1 - sizeof(struct setup_header) */
    pub edd_mbr_sig_buffer: [c_uint; 16],                                      /* 0x290 */
    pub e820_table: [BootE820Entry; 128],                                      /* 0x2d0 */
    pub _pad8: [c_uchar; 48],                                                  /* 0xcd0 */
    pub eddbuf: [EddInfo; 6],                                                  /* 0xd00 */
    pub _pad9: [c_uchar; 276],                                                 /* 0xeec */
}

impl core::fmt::Debug for ArchBootParams {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Struct ArchBootParams(x86) do not support Debug!")
    }
}

// Linux 非0的字段有：
// Sceen_info(为0不影响)
// IstInfo(为0不影响)
// acpi_rsdp_addr(为0不影响)
// alt_mem_k  0x7fb40
// scratch 0x10000d
// e820_entries 0x09
// SetupHeader(重要！非常重要)
// e820_table(与上面的e820_entries数量对应)(这个就是/sys/firmware/memmap)
impl ArchBootParams {
    pub const DEFAULT: Self =
        unsafe { core::mem::MaybeUninit::<ArchBootParams>::zeroed().assume_init() };

    pub fn set_alt_mem_k(&mut self, alt_mem_k: u32) {
        self.alt_mem_k = alt_mem_k;
    }

    pub fn set_scratch(&mut self, scratch: u32) {
        self.scratch = scratch;
    }

    pub fn add_e820_entry(&mut self, addr: u64, size: u64, mtype: u32) {
        let entry = BootE820Entry {
            addr,
            size,
            type_: mtype,
        };
        self.e820_entries += 1;
        self.e820_table[self.e820_entries as usize] = entry;
    }

    pub fn init_setupheader(&mut self) {
        // 不设置就为0
        // 下面的是根据同等 qemu 环境(日期为2025.10.15)在启动 Linux 的值
        // 应该改成自己内核在初始化的过程中获得的值(部分值是需要写死的, 但不应该全部写死)
        self.hdr.setup_sects = 0x40;
        self.hdr.root_flags = 0xfb07;
        self.hdr.syssize = 0x00000d00;
        self.hdr.ram_size = 0x1000;
        self.hdr.vid_mode = 0x09;
        self.hdr.jump = 0xaa55;
        self.hdr.header = 0x53726448;
        self.hdr.version = 0x020f;
        self.hdr.start_sys_seg = 0x1000;
        self.hdr.kernel_version = 0x42a0;
        self.hdr.type_of_loader = 0xb0;
        self.hdr.loadflags = 0x83;
        self.hdr.setup_move_size = 0x8000;
        self.hdr.code32_start = 0x10000000;
        self.hdr.ramdisk_image = 0x00100000;
        self.hdr.ramdisk_size = 0x1eee6000;
        self.hdr.bootsect_kludge = 0x010e9eb0;
        self.hdr.heap_end_ptr = 0xfe00;
        self.hdr.cmd_line_ptr = 0x20000;
        self.hdr.initrd_addr_max = 0x7fffffff;
        self.hdr.kernel_alignment = 0x00200000;
        self.hdr.relocatable_kernel = 0x1;
        self.hdr.min_alignment = 0x15;
        self.hdr.xloadflags = 0x007f;
        self.hdr.cmdline_size = 0x7ff;
    }

    pub fn convert_to_buf(&self) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts(
                (self as *const Self) as *const u8,
                core::mem::size_of::<Self>(),
            )
        }
    }
}
