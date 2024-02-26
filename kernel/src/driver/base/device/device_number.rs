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

    pub const fn new(x: u32) -> Self {
        Major(x)
    }
    pub const fn data(&self) -> u32 {
        self.0
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceNumber {
    data: u32,
}

impl DeviceNumber {
    pub const MINOR_BITS: u32 = 20;
    pub const MINOR_MASK: u32 = 1 << Self::MINOR_BITS - 1;

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
