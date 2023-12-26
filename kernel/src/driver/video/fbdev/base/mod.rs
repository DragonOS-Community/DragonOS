use alloc::{string::String, sync::Arc};
use system_error::SystemError;

use crate::{
    driver::base::device::Device,
    mm::{ucontext::LockedVMA, PhysAddr},
};

pub mod fbcon;
pub mod fbmem;

/// 帧缓冲区应该实现的接口
pub trait FrameBuffer: FrameBufferInfo + FrameBufferOps + Device {}

/// 帧缓冲区信息
pub trait FrameBufferInfo {
    /// Amount of ioremapped VRAM or 0
    fn screen_size(&self) -> usize;

    /// 获取当前的可变帧缓冲信息
    fn current_fb_var(&self) -> &FbVarScreenInfo;

    /// 获取当前的可变帧缓冲信息（可变引用）
    fn current_fb_var_mut(&mut self) -> &mut FbVarScreenInfo;

    /// 获取当前的固定帧缓冲信息
    fn current_fb_fix(&self) -> &FixedScreenInfo;

    /// 获取当前的固定帧缓冲信息（可变引用）
    fn current_fb_fix_mut(&mut self) -> &mut FixedScreenInfo;

    /// 获取当前的视频模式
    fn video_mode(&self) -> Option<&FbVideoMode>;
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
    /// 帧缓冲区的高度（像素）
    pub height: u32,
    /// 帧缓冲区的宽度（像素）
    pub width: u32,
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

#[allow(dead_code)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum FbPixelFormat {
    Standard,
    /// Hold And Modify
    HAM,
    /// order of pixels in each byte is reversed
    Reserved,
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
    pub smem_start: PhysAddr,
    // 帧缓冲区的长度
    pub smem_len: u32,
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
    // 内存映射I/O的起始物理地址
    pub mmio_start: PhysAddr,
    // 内存映射I/O的长度
    pub mmio_len: u32,
    // 表示驱动器拥有的特定芯片/卡片类型
    pub accel: u32,
    // 表示支持的特性，参考 FB_CAP_
    pub capabilities: FbCapability,
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
