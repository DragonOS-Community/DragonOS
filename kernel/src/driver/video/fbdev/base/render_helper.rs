use core::{ops::Add, slice::Iter};

use crate::mm::VirtAddr;

use super::FbImage;

pub struct BitIter<'a> {
    fgcolor: u32,
    bkcolor: u32,
    _color_pattern: EndianPattern,
    _dst_pattern: EndianPattern,
    src: Iter<'a, u8>,
    read_mask: u8,
    byte_per_pixel: u32,
    buffer: u32,
    current: u8,
    left_byte: u32,
    done: bool,
    consumed_bit: u32,
    image_width: u32,
}

impl<'a> BitIter<'a> {
    pub fn new(
        fgcolor: u32,
        bkcolor: u32,
        dst_pattern: EndianPattern,
        color_pattern: EndianPattern,
        byte_per_pixel: u32,
        src: Iter<'a, u8>,
        image_width: u32,
    ) -> Self {
        let mut fgcolor = fgcolor;
        let mut bkcolor = bkcolor;
        if dst_pattern != color_pattern {
            fgcolor = Self::reverse(fgcolor, byte_per_pixel);
            bkcolor = Self::reverse(bkcolor, byte_per_pixel);
        }

        let mut ans = Self {
            fgcolor,
            bkcolor,
            _color_pattern: color_pattern,
            _dst_pattern: dst_pattern,
            src,
            read_mask: 0b10000000,
            byte_per_pixel,
            buffer: 0,
            current: 0,
            left_byte: 0,
            done: false,
            consumed_bit: 0,
            image_width,
        };
        ans.current = *ans.src.next().unwrap();
        return ans;
    }

    fn reverse(num: u32, byte_per_pixel: u32) -> u32 {
        let mask = 0x000000ff;
        let mut ans = 0;
        let mut num = num;
        for _ in 0..3 {
            ans |= mask & num;
            ans <<= 8;
            num >>= 8;
        }
        ans |= mask & num;
        ans >>= (4 - byte_per_pixel) * 8;
        return ans;
    }

    fn move_mask(&mut self) -> bool {
        self.consumed_bit += 1;
        self.read_mask >>= 1;
        if self.read_mask == 0b000000000 {
            self.read_mask = 0b10000000;
            self.current = match self.src.next() {
                Some(x) => *x,
                None => {
                    return false;
                }
            };
            return true;
        } else {
            return true;
        }
    }

    fn full_buffer(&mut self) -> Result<PixelLineStatus, PixelLineStatus> {
        let same_endian = if self._dst_pattern == self._color_pattern {
            1
        } else {
            -1
        };
        let mut color = self.read_bit() << (self.left_byte << 3);
        let mut buffer_pointer = if self._dst_pattern == self._color_pattern {
            0
        } else {
            3
        };
        let mask = 0x000000ff << ((self.byte_per_pixel - 1) << 3);
        let mut temp;
        // while buffer_pointer >= 0 && buffer_pointer <= 3 {
        while (0..=3).contains(&buffer_pointer) {
            if self.consumed_bit >= self.image_width {
                self.consumed_bit = 0;
                return Ok(PixelLineStatus::Full(self.buffer));
            }
            temp = color & mask;
            color <<= 8;
            temp <<= (4 - self.byte_per_pixel) * 8;
            temp >>= buffer_pointer * 8;
            self.buffer |= temp;
            buffer_pointer += same_endian;
            self.left_byte += 1;
            if self.left_byte >= self.byte_per_pixel {
                self.left_byte = 0;
                if !self.move_mask() {
                    return Err(PixelLineStatus::Full(self.buffer));
                }
                color = self.read_bit();
            }
        }
        if self.consumed_bit >= self.image_width {
            self.consumed_bit = 0;
            return Ok(PixelLineStatus::Full(self.buffer));
        }
        return Ok(PixelLineStatus::NotFull(self.buffer));
    }

    fn read_bit(&self) -> u32 {
        match self.read_mask & self.current {
            0 => self.bkcolor,
            _ => self.fgcolor,
        }
    }
}

impl Iterator for BitIter<'_> {
    type Item = (u32, bool);
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.full_buffer() {
            Ok(x) => {
                self.buffer = 0;
                return Some(x.unwarp());
            }
            Err(x) => {
                self.done = true;
                return Some(x.unwarp());
            }
        }
    }
}
#[derive(PartialEq, PartialOrd)]
pub enum EndianPattern {
    Big,
    Little,
}

pub enum PixelLineStatus {
    Full(u32),
    NotFull(u32),
}

impl PixelLineStatus {
    pub fn unwarp(self) -> (u32, bool) {
        match self {
            PixelLineStatus::Full(x) => (x, true),
            PixelLineStatus::NotFull(x) => (x, false),
        }
    }
}

/// # 结构功能
/// 安全的FrameBufferPointer
/// 使用该结构体访问FrameBuffer可以防止超出FrameBuffer区域的访问
/// 需要注意，使用该指针写入时，任何超出屏幕的写入都是无效的！即使仍然可以写入显存。
/// 此外由于FbImage中的x和y变量采用u32类型，所以并未考虑左越界和上越界的安全性(即Image.x<0或Image.y<0的情况)
/// ## 成员
///
///  - "dst" : 显存base address，通常是0xffffa1003ff00000
///  - "limit" : 显存区域上界，可以通过公式计算：limit = dst + 分辨率高*分辨率宽*每个像素的**字节**数。也就是说任何对于显存的访问应该限制在[dst,limit)中
///  - "current" : 当前相对于start_offset的位移
///  - "start_offset" : 如果你要渲染某个Image，你可能不是总是从屏幕左上角(0,0)开始渲染，你可能从某个offset开始
///  - "start_xpos" : 表示你要渲染的Image的x位置的字节位置
///  - "current_xpos" : 当前渲染的x位置的字节位置
///  - "limit_xpos" : 最大的渲染x位置的字节位置。 例：假设系统的分辨率为640，位深为24，你需要渲染的Image的x坐标为200，那么start_xpos=200*3=600，current_xpos=200*3+当前行已经渲染像素数*3，limit_xpos=640*3
#[derive(Debug)]
pub struct FrameP {
    dst: VirtAddr,
    limit: VirtAddr,
    current: usize,
    start_offset: usize,
    start_xpos: usize,
    current_xpos: usize,
    limit_xpos: usize,
}

impl FrameP {
    pub fn new(
        frame_height: usize,
        frame_width: usize,
        bitdepth: usize,
        dst: VirtAddr,
        image: &FbImage,
    ) -> Self {
        let byte_per_pixel = bitdepth / 8;
        let limit = VirtAddr::new(frame_height * frame_width * byte_per_pixel) + dst;
        Self {
            dst,
            limit,
            current: 0,
            start_offset: start_offset(image, bitdepth as u32, (frame_width * bitdepth / 8) as u32)
                as usize,
            start_xpos: image.x as usize * byte_per_pixel,
            current_xpos: image.x as usize * byte_per_pixel,
            limit_xpos: frame_width * byte_per_pixel,
        }
    }

    /// # 函数功能
    /// 写入某个数据并将指针增大
    pub fn write<T>(&mut self, data: T) -> FramePointerStatus {
        // 首先获取数据大小
        let size = size_of::<T>();
        // 复制显存指针防止改变self的dst
        let mut dst = self.dst;

        // 你最终要写入的位置实际上是[dst+start_offset+current,dst+start_offset+current+size),所以我们要确定你写入的位置是否超过limit
        if self.dst.data() + self.current + self.start_offset + size > self.limit.data() {
            return FramePointerStatus::OutOfBuffer;
        }
        // 我们也不希望你的x超出屏幕右边，超出屏幕右边的部分会被忽略掉，因为如果写入显存会导致内存风险
        else if self.current_xpos + size > self.limit_xpos {
            return FramePointerStatus::OutOfScreen;
        }
        // 如果上面两个检查都通过了，我们就可以写入显存了
        else {
            // 这里是写入位置的实际虚拟地址
            dst = dst.add(self.current + self.start_offset);
        }
        // 写入操作
        unsafe {
            *dst.as_ptr::<T>() = data;
        }
        // 写入后更新current和xpos
        self.current += size;
        self.current_xpos += size;
        // 由于写入正常，我们返回正常的状态
        return FramePointerStatus::Normal;
    }

    /// # 函数功能
    /// 移动指针**至**某个offset
    /// todo: 当前函数应当只用于换行，否则可能会导致安全性问题，即offset应该是每行像素的开头
    pub fn move_with_offset(&mut self, offset: u32) {
        self.current = offset as usize;
        // let x_pos=self.current%self.limit_xpos;
        // match x_pos{
        //     n if n<self.start_xpos=>{
        //         // send_to_default_serial8250_port(format!("Sended by function move_with_offset: Check if there is misusage of offset,the image.x is:{:?} while the xpos indicated by the offset is:{:?},current FP:{:?}\n",self.start_offset,x_pos,self).as_bytes());
        //     }
        //     n if n>=self.limit_xpos=>{
        //         // send_to_default_serial8250_port(format!("Sended by function move_with_offset: Check if there is misusage of offset,The offset:{:?} is so large that it would exceed the limit of frame buffer\n",offset).as_bytes());
        //     }
        //     _=>{

        //     }
        // }
        self.current_xpos = self.start_xpos;
    }
}

pub enum FramePointerStatus {
    /// 表示状态正常
    Normal,
    /// 超出屏幕，一直到换行时才应该恢复到正常状态
    OutOfScreen,
    /// 超出缓存，此时应当立即停止渲染
    OutOfBuffer,
}

pub fn start_offset(image: &FbImage, bitdepth: u32, line_length: u32) -> u32 {
    let x = image.x;
    let y = image.y;
    let mut bitstart = (y * line_length * 8) + (x * bitdepth);
    let byte_per_pixel = core::mem::size_of::<u32>() as u32;
    // 位转字节
    bitstart /= 8;

    // 对齐到像素字节大小
    bitstart &= !(byte_per_pixel - 1);
    return bitstart;
}
