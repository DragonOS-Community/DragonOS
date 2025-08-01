use core::{
    fmt::Display,
    hash::{Hash, Hasher},
};

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Major(u32);

impl Major {
    // 常量定义参考:
    //
    // https://code.dragonos.org.cn/xref/linux-6.1.9/include/uapi/linux/major.h

    /// 未命名的主设备
    pub const UNNAMED_MAJOR: Self = Self::new(0);

    pub const IDE0_MAJOR: Self = Self::new(3);
    pub const TTY_MAJOR: Self = Self::new(4);
    pub const TTYAUX_MAJOR: Self = Self::new(5);
    pub const HD_MAJOR: Self = Self::IDE0_MAJOR;

    pub const INPUT_MAJOR: Self = Self::new(13);
    /// /dev/fb* framebuffers
    pub const FB_MAJOR: Self = Self::new(29);

    /// Pty
    pub const UNIX98_PTY_MASTER_MAJOR: Self = Self::new(128);
    pub const UNIX98_PTY_MAJOR_COUNT: Self = Self::new(8);
    pub const UNIX98_PTY_SLAVE_MAJOR: Self =
        Self::new(Self::UNIX98_PTY_MASTER_MAJOR.0 + Self::UNIX98_PTY_MAJOR_COUNT.0);

    /// Disk
    pub const AHCI_BLK_MAJOR: Self = Self::new(8);
    pub const VIRTIO_BLK_MAJOR: Self = Self::new(254);

    pub const HVC_MAJOR: Self = Self::new(229);

    pub const fn new(x: u32) -> Self {
        Major(x)
    }
    pub const fn data(&self) -> u32 {
        self.0
    }
}

impl Hash for Major {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state); // 使用 Major 内部的 u32 值来计算哈希值
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceNumber {
    data: u32,
}

impl DeviceNumber {
    pub const MINOR_BITS: u32 = 20;
    pub const MINOR_MASK: u32 = (1 << Self::MINOR_BITS) - 1;

    pub const fn new(major: Major, minor: u32) -> Self {
        Self {
            data: (major.data() << Self::MINOR_BITS) | minor,
        }
    }

    pub const fn major(&self) -> Major {
        Major::new(self.data >> Self::MINOR_BITS)
    }

    pub const fn minor(&self) -> u32 {
        self.data & 0xfffff
    }

    pub const fn data(&self) -> u32 {
        self.data
    }

    /// acceptable for old filesystems
    pub const fn old_valid_dev(&self) -> bool {
        (self.major().data() < 256) && (self.minor() < 256)
    }

    pub const fn new_encode_dev(&self) -> u32 {
        let major = self.major().data();
        let minor = self.minor();
        return (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12);
    }
}

impl Default for DeviceNumber {
    fn default() -> Self {
        Self::new(Major::UNNAMED_MAJOR, 0)
    }
}

impl From<u32> for DeviceNumber {
    fn from(x: u32) -> Self {
        Self { data: x }
    }
}

impl Display for DeviceNumber {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}:{}", self.major().data(), self.minor())
    }
}
