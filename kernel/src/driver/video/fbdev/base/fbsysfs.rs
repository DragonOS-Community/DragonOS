use alloc::sync::Arc;
use log::warn;
use system_error::SystemError;

use crate::{
    driver::base::kobject::KObject,
    filesystem::{
        sysfs::{file::sysfs_emit_str, Attribute, AttributeGroup, SysFSOpsSupport},
        vfs::syscall::ModeType,
    },
};

use super::fbmem::FbDevice;

/// 为FbDevice实现的sysfs属性组
#[derive(Debug)]
pub struct FbDeviceAttrGroup;

impl AttributeGroup for FbDeviceAttrGroup {
    fn name(&self) -> Option<&str> {
        None
    }

    fn attrs(&self) -> &[&'static dyn Attribute] {
        &[
            &AttrBitsPerPixel,
            &AttrBlank,
            &AttrMode,
            &AttrModes,
            &AttrName,
            &AttrPan,
            &AttrRotate,
            &AttrState,
            &AttrStride,
            &AttrVirtualSize,
        ]
    }

    fn is_visible(
        &self,
        _kobj: alloc::sync::Arc<dyn KObject>,
        _attr: &'static dyn Attribute,
    ) -> Option<ModeType> {
        None
    }
}

#[derive(Debug)]
struct AttrName;

impl Attribute for AttrName {
    fn name(&self) -> &str {
        "name"
    }

    fn mode(&self) -> ModeType {
        ModeType::S_IRUGO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let fb_dev = kobj.arc_any().downcast::<FbDevice>().unwrap();
        let fb = fb_dev.framebuffer().ok_or(SystemError::ENODEV)?;
        let name = fb.name();
        return sysfs_emit_str(buf, &format!("{}\n", name));
    }
}

#[derive(Debug)]
struct AttrBitsPerPixel;

impl Attribute for AttrBitsPerPixel {
    fn name(&self) -> &str {
        "bits_per_pixel"
    }

    fn mode(&self) -> ModeType {
        ModeType::S_IRUGO | ModeType::S_IWUSR
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW | SysFSOpsSupport::ATTR_STORE
    }

    fn store(&self, _kobj: Arc<dyn KObject>, _buf: &[u8]) -> Result<usize, SystemError> {
        warn!("attr bits_per_pixel store not implemented");
        return Err(SystemError::ENOSYS);
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let fb_dev = kobj.arc_any().downcast::<FbDevice>().unwrap();
        let fb = fb_dev.framebuffer().ok_or(SystemError::ENODEV)?;
        let bits_per_pixel = fb.current_fb_var().bits_per_pixel;
        return sysfs_emit_str(buf, &format!("{}\n", bits_per_pixel));
    }
}

/// 用于清空屏幕的属性
#[derive(Debug)]
struct AttrBlank;

impl Attribute for AttrBlank {
    fn name(&self) -> &str {
        "blank"
    }

    fn mode(&self) -> ModeType {
        ModeType::S_IWUSR
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_STORE
    }

    // todo:  https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/video/fbdev/core/fbsysfs.c#309
    fn store(&self, _kobj: Arc<dyn KObject>, _buf: &[u8]) -> Result<usize, SystemError> {
        warn!("attr blank store not implemented");
        return Err(SystemError::ENOSYS);
    }
}

#[derive(Debug)]
struct AttrMode;

impl Attribute for AttrMode {
    fn name(&self) -> &str {
        "mode"
    }

    fn mode(&self) -> ModeType {
        ModeType::S_IRUGO | ModeType::S_IWUSR
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW | SysFSOpsSupport::ATTR_STORE
    }

    /// https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/video/fbdev/core/fbsysfs.c#166
    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        todo!("AttrMode::show")
    }

    /// https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/video/fbdev/core/fbsysfs.c#135
    fn store(&self, _kobj: Arc<dyn KObject>, _buf: &[u8]) -> Result<usize, SystemError> {
        todo!("AttrMode::store")
    }
}

#[derive(Debug)]
struct AttrModes;

impl Attribute for AttrModes {
    fn name(&self) -> &str {
        "modes"
    }

    fn mode(&self) -> ModeType {
        ModeType::S_IRUGO | ModeType::S_IWUSR
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW | SysFSOpsSupport::ATTR_STORE
    }

    /// https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/video/fbdev/core/fbsysfs.c#206
    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        todo!("AttrMode::show")
    }

    /// https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/video/fbdev/core/fbsysfs.c#177
    fn store(&self, _kobj: Arc<dyn KObject>, _buf: &[u8]) -> Result<usize, SystemError> {
        todo!("AttrMode::store")
    }
}

#[derive(Debug)]
struct AttrPan;

impl Attribute for AttrPan {
    fn name(&self) -> &str {
        "pan"
    }

    fn mode(&self) -> ModeType {
        ModeType::S_IRUGO | ModeType::S_IWUSR
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW | SysFSOpsSupport::ATTR_STORE
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let fb_dev = kobj.arc_any().downcast::<FbDevice>().unwrap();
        let fb = fb_dev.framebuffer().ok_or(SystemError::ENODEV)?;
        let var_info = fb.current_fb_var();
        return sysfs_emit_str(buf, &format!("{},{}\n", var_info.xoffset, var_info.yoffset));
    }

    /// https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/video/fbdev/core/fbsysfs.c#365
    fn store(&self, _kobj: Arc<dyn KObject>, _buf: &[u8]) -> Result<usize, SystemError> {
        todo!("AttrPan::store")
    }
}

#[derive(Debug)]
struct AttrVirtualSize;

impl Attribute for AttrVirtualSize {
    fn name(&self) -> &str {
        "virtual_size"
    }

    fn mode(&self) -> ModeType {
        ModeType::S_IRUGO | ModeType::S_IWUSR
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW | SysFSOpsSupport::ATTR_STORE
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let fb_dev = kobj.arc_any().downcast::<FbDevice>().unwrap();
        let fb = fb_dev.framebuffer().ok_or(SystemError::ENODEV)?;
        let var_info = fb.current_fb_var();
        return sysfs_emit_str(
            buf,
            &format!("{},{}\n", var_info.xres_virtual, var_info.yres_virtual),
        );
    }

    /// https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/video/fbdev/core/fbsysfs.c#273
    fn store(&self, _kobj: Arc<dyn KObject>, _buf: &[u8]) -> Result<usize, SystemError> {
        todo!("AttrVirtualSize::store")
    }
}

#[derive(Debug)]
struct AttrStride;

impl Attribute for AttrStride {
    fn name(&self) -> &str {
        "stride"
    }

    fn mode(&self) -> ModeType {
        ModeType::S_IRUGO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let fb_dev = kobj.arc_any().downcast::<FbDevice>().unwrap();
        let fb = fb_dev.framebuffer().ok_or(SystemError::ENODEV)?;
        let fix_info = fb.current_fb_fix();
        return sysfs_emit_str(buf, &format!("{}\n", fix_info.line_length));
    }
}

#[derive(Debug)]
struct AttrRotate;

impl Attribute for AttrRotate {
    fn name(&self) -> &str {
        "rotate"
    }

    fn mode(&self) -> ModeType {
        ModeType::S_IRUGO | ModeType::S_IWUSR
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW | SysFSOpsSupport::ATTR_STORE
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let fb_dev = kobj.arc_any().downcast::<FbDevice>().unwrap();
        let fb = fb_dev.framebuffer().ok_or(SystemError::ENODEV)?;
        let var_info = fb.current_fb_var();

        return sysfs_emit_str(buf, &format!("{}\n", var_info.rotate_angle));
    }

    /// todo https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/video/fbdev/core/fbsysfs.c#246
    fn store(&self, _kobj: Arc<dyn KObject>, _buf: &[u8]) -> Result<usize, SystemError> {
        todo!("AttrRotate::store")
    }
}

#[derive(Debug)]
struct AttrState;

impl Attribute for AttrState {
    fn name(&self) -> &str {
        "state"
    }

    fn mode(&self) -> ModeType {
        ModeType::S_IRUGO | ModeType::S_IWUSR
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW | SysFSOpsSupport::ATTR_STORE
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let fb_dev = kobj.arc_any().downcast::<FbDevice>().unwrap();
        let fb = fb_dev.framebuffer().ok_or(SystemError::ENODEV)?;

        return sysfs_emit_str(buf, &format!("{}\n", fb.state() as u8));
    }

    /// todo https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/video/fbdev/core/fbsysfs.c#406
    fn store(&self, _kobj: Arc<dyn KObject>, _buf: &[u8]) -> Result<usize, SystemError> {
        todo!("AttrState::store")
    }
}
