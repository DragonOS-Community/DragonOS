use alloc::{string::String, sync::Arc};
use system_error::SystemError;

use crate::{
    driver::base::device::Device,
    mm::{ucontext::LockedVMA, PhysAddr, VirtAddr},
};

use self::fbmem::{FbDevice, FrameBufferManager};

pub mod fbcon;
pub mod fbmem;
pub mod fbsysfs;
pub mod modedb;

// 帧缓冲区id
int_like!(FbId, u32);

impl FbId {
    /// 帧缓冲区id的初始值（无效值）
    pub const INIT: Self = Self::new(u32::MAX);

    /// 判断是否为无效的帧缓冲区id
    #[allow(dead_code)]
    pub const fn is_valid(&self) -> bool {
        if self.0 == Self::INIT.0 || self.0 >= FrameBufferManager::FB_MAX as u32 {
            return false;
        }
        return true;
    }
}

/// 帧缓冲区应该实现的接口
pub trait FrameBuffer: FrameBufferInfo + FrameBufferOps + Device {
    /// 获取帧缓冲区的id
    fn fb_id(&self) -> FbId;

    /// 设置帧缓冲区的id
    fn set_fb_id(&self, id: FbId);
}

/// 帧缓冲区信息
pub trait FrameBufferInfo {
    /// Amount of ioremapped VRAM or 0
    fn screen_size(&self) -> usize;

    /// 获取当前的可变帧缓冲信息
    fn current_fb_var(&self) -> FbVarScreenInfo;

    /// 获取当前的固定帧缓冲信息
    fn current_fb_fix(&self) -> FixedScreenInfo;

    /// 获取当前的视频模式
    fn video_mode(&self) -> Option<&FbVideoMode>;

    /// 获取当前帧缓冲区对应的`/sys/class/graphics/fb0`或者`/sys/class/graphics/fb1`等的设备结构体
    fn fb_device(&self) -> Option<Arc<FbDevice>>;

    /// 设置当前帧缓冲区对应的`/sys/class/graphics/fb0`或者`/sys/class/graphics/fb1`等的设备结构体
    fn set_fb_device(&self, device: Option<Arc<FbDevice>>);

    /// 获取帧缓冲区的状态
    fn state(&self) -> FbState;
}

/// 帧缓冲区操作
///
/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/fb.h#237
pub trait FrameBufferOps {
    fn fb_open(&self, user: bool);
    fn fb_release(&self, user: bool);

    /// 读取帧缓冲区的内容。
    ///
    /// 对于具有奇特非线性布局的帧缓冲区或正常内存映射访问无法工作的帧缓冲区，可以使用此方法。
    fn fb_read(&self, _buf: &mut [u8], _pos: usize) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// 将帧缓冲区的内容写入。
    ///
    /// 对于具有奇特非线性布局的帧缓冲区或正常内存映射访问无法工作的帧缓冲区，可以使用此方法。
    fn fb_write(&self, _buf: &[u8], _pos: usize) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// 设置帧缓冲区的颜色寄存器。
    ///
    /// 颜色寄存器的数量和含义取决于帧缓冲区的硬件。
    ///
    /// ## 参数
    ///
    /// - `regno`：寄存器编号。
    /// - `red`：红色分量。
    /// - `green`：绿色分量。
    /// - `blue`：蓝色分量。
    fn fb_set_color_register(
        &self,
        regno: u16,
        red: u16,
        green: u16,
        blue: u16,
    ) -> Result<(), SystemError>;

    /// 设置帧缓冲区的黑屏模式
    fn fb_blank(&self, blank_mode: BlankMode) -> Result<(), SystemError>;

    /// 在帧缓冲区中绘制一个矩形。
    fn fb_fillrect(&self, _data: FillRectData) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// 将数据从一处复制到另一处。
    fn fb_copyarea(&self, _data: CopyAreaData) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// 将帧缓冲区的内容映射到用户空间。
    fn fb_mmap(&self, _vma: &Arc<LockedVMA>) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// 卸载与该帧缓冲区相关的所有资源
    fn fb_destroy(&self);
}

/// 帧缓冲区的状态
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum FbState {
    Running = 0,
    Suspended = 1,
}

/// 屏幕黑屏模式。
#[allow(dead_code)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum BlankMode {
    /// 取消屏幕黑屏, 垂直同步和水平同步均打开
    Unblank,
    /// 屏幕黑屏, 垂直同步和水平同步均打开
    Normal,
    /// 屏幕黑屏, 水平同步打开, 垂直同步关闭
    HSync,
    /// 屏幕黑屏, 水平同步关闭, 垂直同步打开
    VSync,
    /// 屏幕黑屏, 水平同步和垂直同步均关闭
    Powerdown,
}

/// `FillRectData` 结构体用于表示一个矩形区域并填充特定颜色。
///
/// # 结构体字段
/// * `dx`:
/// * `dy`:
/// * `width`:
/// * `height`: 矩形的高度
/// * `color`: 用于填充矩形的颜色，是一个32位无符号整数
/// * `rop`: 光栅操作（Raster Operation），用于定义如何将颜色应用到矩形区域
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct FillRectData {
    /// 矩形左上角的x坐标（相对于屏幕）
    pub dx: u32,
    /// 矩形左上角的y坐标（相对于屏幕）
    pub dy: u32,
    /// 矩形的宽度
    pub width: u32,
    /// 矩形的高度
    pub height: u32,
    /// 用于填充矩形的颜色，是一个32位无符号整数
    pub color: u32,
    /// 光栅操作（Raster Operation），用于定义如何将颜色应用到矩形区域
    pub rop: FillRectROP,
}

impl FillRectData {
    #[allow(dead_code)]
    pub fn new(dx: u32, dy: u32, width: u32, height: u32, color: u32, rop: FillRectROP) -> Self {
        Self {
            dx,
            dy,
            width,
            height,
            color,
            rop,
        }
    }
}

/// 光栅操作（Raster Operation），用于定义如何将颜色应用到矩形区域
#[allow(dead_code)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum FillRectROP {
    /// 复制操作，即直接将指定颜色应用到矩形区域，覆盖原有颜色。
    Copy,
    /// 异或操作，即将指定颜色与矩形区域原有颜色进行异或操作，结果颜色应用到矩形区域。
    Xor,
}

/// `CopyAreaData` 结构体用于表示一个矩形区域，并指定从哪个源位置复制数据。
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct CopyAreaData {
    /// 目标矩形左上角的x坐标
    pub dx: u32,
    /// 目标矩形左上角的y坐标
    pub dy: u32,
    /// 矩形的宽度
    pub width: u32,
    /// 矩形的高度
    pub height: u32,
    /// 源矩形左上角的x坐标
    pub sx: u32,
    /// 源矩形左上角的y坐标
    pub sy: u32,
}

impl CopyAreaData {
    #[allow(dead_code)]
    pub fn new(dx: u32, dy: u32, width: u32, height: u32, sx: u32, sy: u32) -> Self {
        Self {
            dx,
            dy,
            width,
            height,
            sx,
            sy,
        }
    }
}

/// `FbVarScreenInfo` 结构体用于描述屏幕的各种属性。
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct FbVarScreenInfo {
    /// 可见分辨率的宽度
    pub xres: u32,
    /// 可见分辨率的高度
    pub yres: u32,
    /// 虚拟分辨率的宽度
    pub xres_virtual: u32,
    /// 虚拟分辨率的高度
    pub yres_virtual: u32,
    /// 从虚拟到可见分辨率的偏移量（宽度方向）
    pub xoffset: u32,
    /// 从虚拟到可见分辨率的偏移量（高度方向）
    pub yoffset: u32,
    /// 每像素的位数
    pub bits_per_pixel: u32,
    /// 颜色模式
    pub color_mode: FbColorMode,
    /// 红色位域
    pub red: FbBitfield,
    /// 绿色位域
    pub green: FbBitfield,
    /// 蓝色位域
    pub blue: FbBitfield,
    /// 透明度位域
    pub transp: FbBitfield,
    /// 像素格式
    pub pixel_format: FbPixelFormat,
    /// 激活标志（参见FB_ACTIVATE_*）
    pub activate: FbActivateFlags,
    /// 帧缓冲区的高度（像素） None表示未知
    pub height: Option<u32>,
    /// 帧缓冲区的宽度（像素） None表示未知
    pub width: Option<u32>,
    /// 像素时钟（皮秒）
    pub pixclock: u32,
    /// 左边距
    pub left_margin: u32,
    /// 右边距
    pub right_margin: u32,
    /// 上边距
    pub upper_margin: u32,
    /// 下边距
    pub lower_margin: u32,
    /// 水平同步的长度
    pub hsync_len: u32,
    /// 垂直同步的长度
    pub vsync_len: u32,
    /// 同步标志（参见FB_SYNC_*）
    pub sync: FbSyncFlags,
    /// 视频模式（参见FB_VMODE_*）
    pub vmode: FbVModeFlags,
    /// 逆时针旋转的角度
    pub rotate_angle: u32,
    /// 颜色空间
    pub colorspace: V4l2Colorspace,
}

impl Default for FbVarScreenInfo {
    fn default() -> Self {
        Self {
            xres: Default::default(),
            yres: Default::default(),
            xres_virtual: Default::default(),
            yres_virtual: Default::default(),
            xoffset: Default::default(),
            yoffset: Default::default(),
            bits_per_pixel: Default::default(),
            color_mode: Default::default(),
            red: Default::default(),
            green: Default::default(),
            blue: Default::default(),
            transp: Default::default(),
            pixel_format: Default::default(),
            activate: Default::default(),
            height: None,
            width: None,
            pixclock: Default::default(),
            left_margin: Default::default(),
            right_margin: Default::default(),
            upper_margin: Default::default(),
            lower_margin: Default::default(),
            hsync_len: Default::default(),
            vsync_len: Default::default(),
            sync: FbSyncFlags::empty(),
            vmode: FbVModeFlags::empty(),
            rotate_angle: Default::default(),
            colorspace: Default::default(),
        }
    }
}

/// 帧缓冲区的颜色模式
///
/// 默认为彩色
#[allow(dead_code)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum FbColorMode {
    /// 灰度
    GrayScale,
    /// 彩色
    Color,
    /// FOURCC
    FourCC,
}

impl Default for FbColorMode {
    fn default() -> Self {
        FbColorMode::Color
    }
}

/// `FbBitfield` 结构体用于描述颜色字段的位域。
///
/// 所有的偏移量都是从右边开始，位于一个精确为'bits_per_pixel'宽度的"像素"值内。
/// 一个像素之后是一个位流，并且未经修改地写入视频内存。
///
/// 对于伪颜色：所有颜色组件的偏移和长度应该相同。
/// 偏移指定了调色板索引在像素值中的最低有效位的位置。
/// 长度表示可用的调色板条目的数量（即条目数 = 1 << 长度）。
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct FbBitfield {
    /// 位域的起始位置
    pub offset: u32,
    /// 位域的长度
    pub length: u32,
    /// 最高有效位是否在右边
    pub msb_right: bool,
}

impl FbBitfield {
    #[allow(dead_code)]
    pub fn new(offset: u32, length: u32, msb_right: bool) -> Self {
        Self {
            offset,
            length,
            msb_right,
        }
    }
}

impl Default for FbBitfield {
    fn default() -> Self {
        Self {
            offset: Default::default(),
            length: Default::default(),
            msb_right: Default::default(),
        }
    }
}

bitflags! {
    /// `FbActivateFlags` 用于描述帧缓冲区的激活标志。
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/include/uapi/linux/fb.h#198
    pub struct FbActivateFlags: u32 {
        /// 立即设置值（或vbl）
        const FB_ACTIVATE_NOW = 0;
        /// 在下一次打开时激活
        const FB_ACTIVATE_NXTOPEN = 1;
        /// don't set, round up impossible values
        const FB_ACTIVATE_TEST = 2;
        const FB_ACTIVATE_MASK = 15;

        /// 在下一个vbl上激活值
        const FB_ACTIVATE_VBL = 16;
        /// 在vbl上更改色彩映射
        const FB_ACTIVATE_CHANGE_CMAP_VBL = 32;
        /// 更改此fb上的所有VC
        const FB_ACTIVATE_ALL = 64;
        /// 即使没有变化也强制应用
        const FB_ACTIVATE_FORCE = 128;
        /// 使视频模式无效
        const FB_ACTIVATE_INV_MODE = 256;
        /// 用于KDSET vt ioctl
        const FB_ACTIVATE_KD_TEXT = 512;
    }
}

impl Default for FbActivateFlags {
    fn default() -> Self {
        FbActivateFlags::FB_ACTIVATE_NOW
    }
}

#[allow(dead_code)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum FbPixelFormat {
    Standard,
    /// Hold And Modify
    HAM,
    /// order of pixels in each byte is reversed
    Reserved,
}

impl Default for FbPixelFormat {
    fn default() -> Self {
        FbPixelFormat::Standard
    }
}

bitflags! {
    pub struct FbSyncFlags: u32 {
        /// 水平同步高电平有效
        const FB_SYNC_HOR_HIGH_ACT = 1;
        /// 垂直同步高电平有效
        const FB_SYNC_VERT_HIGH_ACT = 2;
        /// 外部同步
        const FB_SYNC_EXT = 4;
        /// 复合同步高电平有效
        const FB_SYNC_COMP_HIGH_ACT = 8;
        /// 广播视频时序
        const FB_SYNC_BROADCAST = 16;
        /// sync on green
        const FB_SYNC_ON_GREEN = 32;
    }
}

bitflags! {
    /// `FbVModeFlags` 用于描述帧缓冲区的视频模式。
    pub struct FbVModeFlags: u32 {
        /// 非交错
        const FB_VMODE_NONINTERLACED = 0;
        /// 交错
        const FB_VMODE_INTERLACED = 1;
        /// 双扫描
        const FB_VMODE_DOUBLE = 2;
        /// 交错：首先是顶行
        const FB_VMODE_ODD_FLD_FIRST = 4;
        /// 掩码
        const FB_VMODE_MASK = 255;
        /// ywrap代替平移
        const FB_VMODE_YWRAP = 256;
        /// 平滑xpan可能（内部使用）
        const FB_VMODE_SMOOTH_XPAN = 512;
        /// 不更新x/yoffset
        const FB_VMODE_CONUPDATE = 512;
    }
}

/// 视频颜色空间
#[allow(dead_code)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum V4l2Colorspace {
    /// 默认颜色空间，即让驱动程序自行判断。只能用于视频捕获。
    Default = 0,
    /// SMPTE 170M：用于广播NTSC/PAL SDTV
    Smpte170m = 1,
    /// 过时的1998年前的SMPTE 240M HDTV标准，已被Rec 709取代
    Smpte240m = 2,
    /// Rec.709：用于HDTV
    Rec709 = 3,
    /// 已弃用，不要使用。没有驱动程序会返回这个。这是基于对bt878数据表的误解。
    Bt878 = 4,
    /// NTSC 1953颜色空间。只有在处理非常非常旧的NTSC录音时才有意义。已被SMPTE 170M取代。
    System470M = 5,
    /// EBU Tech 3213 PAL/SECAM颜色空间。
    System470Bg = 6,
    /// 实际上是V4L2_COLORSPACE_SRGB，V4L2_YCBCR_ENC_601和V4L2_QUANTIZATION_FULL_RANGE的简写。用于(Motion-)JPEG。
    Jpeg = 7,
    /// 用于RGB颜色空间，如大多数网络摄像头所产生的。
    Srgb = 8,
    /// opRGB颜色空间
    Oprgb = 9,
    /// BT.2020颜色空间，用于UHDTV。
    Bt2020 = 10,
    /// Raw颜色空间：用于RAW未处理的图像
    Raw = 11,
    /// DCI-P3颜色空间，用于电影投影机
    DciP3 = 12,

    /// Largest supported colorspace value, assigned by the compiler, used
    /// by the framework to check for invalid values.
    Last,
}

impl Default for V4l2Colorspace {
    fn default() -> Self {
        V4l2Colorspace::Default
    }
}

/// `FixedScreenInfo` 结构体用于描述屏幕的固定属性。
#[allow(dead_code)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct FixedScreenInfo {
    // 字符串，用于标识屏幕，例如 "TT Builtin"
    pub id: [char; 16],
    // 帧缓冲区的起始物理地址
    pub smem_start: Option<PhysAddr>,
    // 帧缓冲区的长度
    pub smem_len: usize,
    // 屏幕类型，参考 FB_TYPE_
    pub fb_type: FbType,
    // 用于表示交错平面的小端辅助类型
    pub type_aux: u32,
    // 视觉类型，参考 FB_VISUAL_
    pub visual: FbVisual,
    // 水平缩放步长，如果无硬件缩放，则为0
    pub xpanstep: u16,
    // 垂直缩放步长，如果无硬件缩放，则为0
    pub ypanstep: u16,
    // 垂直环绕步长，如果无硬件环绕，则为0
    pub ywrapstep: u16,
    // 一行的大小（以字节为单位）
    pub line_length: u32,
    // 内存映射I/O端口的起始物理地址
    pub mmio_start: Option<PhysAddr>,
    // 内存映射I/O的长度
    pub mmio_len: usize,
    // 表示驱动器拥有的特定芯片/卡片类型
    pub accel: FbAccel,
    // 表示支持的特性，参考 FB_CAP_
    pub capabilities: FbCapability,
}

impl FixedScreenInfo {
    /// 将字符串转换为长度为16的字符数组（包含结尾的`\0`）
    ///
    /// ## 参数
    ///
    /// - `name`: 字符串,长度不超过15，超过的部分将被截断
    ///
    /// ## 返回
    ///
    /// 长度为16的字符数组
    pub const fn name2id(name: &str) -> [char; 16] {
        let mut id = [0 as char; 16];
        let mut i = 0;

        while i < 15 && i < name.len() {
            id[i] = name.as_bytes()[i] as char;
            i += 1;
        }

        id[i] = '\0';
        return id;
    }
}

impl Default for FixedScreenInfo {
    fn default() -> Self {
        Self {
            id: Default::default(),
            smem_start: None,
            smem_len: Default::default(),
            fb_type: FbType::PackedPixels,
            type_aux: Default::default(),
            visual: FbVisual::Mono10,
            xpanstep: Default::default(),
            ypanstep: Default::default(),
            ywrapstep: Default::default(),
            line_length: Default::default(),
            mmio_start: None,
            mmio_len: Default::default(),
            accel: Default::default(),
            capabilities: Default::default(),
        }
    }
}

/// 帧缓冲类型
#[allow(dead_code)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum FbType {
    /// 压缩像素
    PackedPixels = 0,
    /// 非交错平面
    Planes = 1,
    /// 交错平面
    InterleavedPlanes = 2,
    /// 文本/属性
    Text = 3,
    /// EGA/VGA平面
    VgaPlanes = 4,
    /// 由V4L2 FOURCC标识的类型
    FourCC = 5,
}

/// 帧缓冲视觉类型
#[allow(dead_code)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum FbVisual {
    /// 单色。1=黑色 0=白色
    Mono01 = 0,
    /// 单色。1=白色 0=黑色
    Mono10 = 1,
    /// 真彩色
    TrueColor = 2,
    /// 伪彩色（如Atari）
    PseudoColor = 3,
    /// 直接颜色
    DirectColor = 4,
    /// 只读的伪彩色
    StaticPseudoColor = 5,
    /// 由FOURCC标识的类型
    FourCC,
}

#[allow(dead_code)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum FbCapability {
    Default = 0,
    /// 设备支持基于FOURCC的格式。
    FourCC,
}

impl Default for FbCapability {
    fn default() -> Self {
        FbCapability::Default
    }
}

/// 视频模式
#[allow(dead_code)]
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct FbVideoMode {
    /// 可选的名称
    pub name: Option<String>,
    /// 可选的刷新率
    pub refresh: Option<u32>,
    /// 水平分辨率
    pub xres: u32,
    /// 垂直分辨率
    pub yres: u32,
    /// 像素时钟
    pub pixclock: u32,
    /// 左边距
    pub left_margin: u32,
    /// 右边距
    pub right_margin: u32,
    /// 上边距
    pub upper_margin: u32,
    /// 下边距
    pub lower_margin: u32,
    /// 水平同步长度
    pub hsync_len: u32,
    /// 垂直同步长度
    pub vsync_len: u32,
    /// 同步
    pub sync: FbSyncFlags,
    /// 视频模式
    pub vmode: FbVModeFlags,
    /// 标志
    pub flag: u32,
}

#[allow(dead_code)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum FbAccel {
    /// 没有硬件加速器
    None,

    AtariBlitter = 1,
    AmigaBlitter = 2,
    S3Trio64 = 3,
    NCR77C32BLT = 4,
    S3Virge = 5,
    AtiMach64GX = 6,
    DECTGA = 7,
    AtiMach64CT = 8,
    AtiMach64VT = 9,
    AtiMach64GT = 10,
    SunCreator = 11,
    SunCGSix = 12,
    SunLeo = 13,
    IMSTwinTurbo = 14,
    Acc3DLabsPermedia2 = 15,
    MatroxMGA2064W = 16,
    MatroxMGA1064SG = 17,
    MatroxMGA2164W = 18,
    MatroxMGA2164WAGP = 19,
    MatroxMGAG400 = 20,
    NV3 = 21,
    NV4 = 22,
    NV5 = 23,
    NV6 = 24,
    XGIVolariV = 25,
    XGIVolariZ = 26,
    Omap1610 = 27,
    TridentTGUI = 28,
    Trident3DImage = 29,
    TridentBlade3D = 30,
    TridentBladeXP = 31,
    CirrusAlpine = 32,
    NeoMagicNM2070 = 90,
    NeoMagicNM2090 = 91,
    NeoMagicNM2093 = 92,
    NeoMagicNM2097 = 93,
    NeoMagicNM2160 = 94,
    NeoMagicNM2200 = 95,
    NeoMagicNM2230 = 96,
    NeoMagicNM2360 = 97,
    NeoMagicNM2380 = 98,
    PXA3XX = 99,

    Savage4 = 0x80,
    Savage3D = 0x81,
    Savage3DMV = 0x82,
    Savage2000 = 0x83,
    SavageMXMV = 0x84,
    SavageMX = 0x85,
    SavageIXMV = 0x86,
    SavageIX = 0x87,
    ProSavagePM = 0x88,
    ProSavageKM = 0x89,
    S3Twister = 0x8a,
    S3TwisterK = 0x8b,
    SuperSavage = 0x8c,
    ProSavageDDR = 0x8d,
    ProSavageDDRK = 0x8e,
    // Add other accelerators here
}

impl Default for FbAccel {
    fn default() -> Self {
        FbAccel::None
    }
}

#[derive(Debug, Copy, Clone)]
pub struct BootTimeScreenInfo {
    pub origin_x: u8,
    pub origin_y: u8,
    /// text mode时，每行的字符数
    pub origin_video_cols: u8,
    /// text mode时，行数
    pub origin_video_lines: u8,
    /// 标记屏幕是否为VGA类型
    pub is_vga: bool,
    /// video mode type
    pub video_type: BootTimeVideoType,

    // 以下字段用于线性帧缓冲区
    /// 线性帧缓冲区的起始物理地址
    pub lfb_base: PhysAddr,
    /// 线性帧缓冲区在初始化阶段被映射到的起始虚拟地址
    ///
    /// 这个值可能会被设置2次：
    ///
    /// - 内存管理初始化之前，early init阶段，临时映射
    /// - 内存管理初始化完毕，重新映射时被设置
    pub lfb_virt_base: Option<VirtAddr>,
    /// 线性帧缓冲区的长度
    pub lfb_size: usize,
    /// 线性帧缓冲区的宽度（像素）
    pub lfb_width: u32,
    /// 线性帧缓冲区的高度（像素）
    pub lfb_height: u32,
    /// 线性帧缓冲区的深度（位数）
    pub lfb_depth: u8,
    /// 红色位域的大小
    pub red_size: u8,
    /// 红色位域的偏移量（左移位数）
    pub red_pos: u8,
    /// 绿色位域的大小
    pub green_size: u8,
    /// 绿色位域的偏移量（左移位数）
    pub green_pos: u8,
    /// 蓝色位域的大小
    pub blue_size: u8,
    /// 蓝色位域的偏移量（左移位数）
    pub blue_pos: u8,
}

impl BootTimeScreenInfo {
    pub const DEFAULT: Self = Self {
        origin_x: 0,
        origin_y: 0,
        is_vga: false,
        lfb_base: PhysAddr::new(0),
        lfb_size: 0,
        lfb_width: 0,
        lfb_height: 0,
        red_size: 0,
        red_pos: 0,
        green_size: 0,
        green_pos: 0,
        blue_size: 0,
        blue_pos: 0,
        video_type: BootTimeVideoType::UnDefined,
        origin_video_cols: 0,
        origin_video_lines: 0,
        lfb_virt_base: None,
        lfb_depth: 0,
    };
}

/// Video types for different display hardware
#[allow(dead_code)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum BootTimeVideoType {
    UnDefined,
    /// Monochrome Text Display
    Mda,
    /// CGA Display
    Cga,
    /// EGA/VGA in Monochrome Mode
    EgaM,
    /// EGA in Color Mode
    EgaC,
    /// VGA+ in Color Mode
    VgaC,
    /// VESA VGA in graphic mode
    Vlfb,
    /// ACER PICA-61 local S3 video
    PicaS3,
    /// MIPS Magnum 4000 G364 video
    MipsG364,
    /// Various SGI graphics hardware
    Sgi,
    /// DEC TGA
    TgaC,
    /// Sun frame buffer
    Sun,
    /// Sun PCI based frame buffer
    SunPci,
    /// PowerMacintosh frame buffer
    Pmac,
    /// EFI graphic mode
    Efi,
}
