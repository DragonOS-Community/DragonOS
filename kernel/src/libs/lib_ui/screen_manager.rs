use core::{
    fmt::Debug,
    intrinsics::unlikely,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use alloc::{boxed::Box, collections::LinkedList, string::String, sync::Arc};
use system_error::SystemError;

use crate::{
    driver::{serial::serial8250::send_to_default_serial8250_port, video::video_refresh_manager},
    libs::{lib_ui::textui::textui_is_enable_put_to_window, rwlock::RwLock, spinlock::SpinLock},
    mm::VirtAddr,
};

use super::{
    textui::{textui_disable_put_to_window, textui_enable_put_to_window},
    textui_no_alloc::textui_init_no_alloc,
};

/// 全局的UI框架列表
pub static SCM_FRAMEWORK_LIST: SpinLock<LinkedList<Arc<dyn ScmUiFramework>>> =
    SpinLock::new(LinkedList::new());

/// 当前在使用的UI框架
pub static CURRENT_FRAMEWORK: RwLock<Option<Arc<dyn ScmUiFramework>>> = RwLock::new(None);

/// 是否启用双缓冲
pub static SCM_DOUBLE_BUFFER_ENABLED: AtomicBool = AtomicBool::new(false);

bitflags! {
  pub struct ScmBufferFlag:u8 {
    // 帧缓冲区标志位
       const SCM_BF_FB = 1 << 0; // 当前buffer是设备显存中的帧缓冲区
       const SCM_BF_DB = 1 << 1; // 当前buffer是双缓冲
       const SCM_BF_TEXT = 1 << 2; // 使用文本模式
       const SCM_BF_PIXEL = 1 << 3; // 使用图像模式
   }
}
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub enum ScmFramworkType {
    Text,
    Gui,
    Unused,
}
#[derive(Debug, Clone)]
pub enum ScmBuffer {
    DeviceBuffer(VirtAddr),
    DoubleBuffer(Arc<SpinLock<Box<[u32]>>>),
}

#[derive(Debug, Clone)]
pub struct ScmBufferInfo {
    width: u32,     // 帧缓冲区宽度（pixel或columns）
    height: u32,    // 帧缓冲区高度（pixel或lines）
    size: u32,      // 帧缓冲区大小（bytes）
    bit_depth: u32, // 像素点位深度
    pub buf: ScmBuffer,
    flags: ScmBufferFlag, // 帧缓冲区标志位
}

#[allow(dead_code)]
impl ScmBufferInfo {
    /// 创建新的帧缓冲区信息
    ///
    /// ## 参数
    ///
    /// - `buf_type` 帧缓冲区类型
    ///
    /// ## 返回值
    ///
    /// - `Result<Self, SystemError>` 创建成功返回新的帧缓冲区结构体，创建失败返回错误码
    pub fn new(mut buf_type: ScmBufferFlag) -> Result<Self, SystemError> {
        if unlikely(SCM_DOUBLE_BUFFER_ENABLED.load(Ordering::SeqCst) == false) {
            let mut device_buffer = video_refresh_manager().device_buffer().clone();
            buf_type.remove(ScmBufferFlag::SCM_BF_DB);
            buf_type.insert(ScmBufferFlag::SCM_BF_FB);
            device_buffer.flags = buf_type;
            return Ok(device_buffer);
        } else {
            let device_buffer_guard = video_refresh_manager().device_buffer();

            let buf_space: Arc<SpinLock<Box<[u32]>>> = Arc::new(SpinLock::new(
                vec![0u32; (device_buffer_guard.size / 4) as usize].into_boxed_slice(),
            ));

            assert!(buf_type.contains(ScmBufferFlag::SCM_BF_DB));

            assert_eq!(
                device_buffer_guard.size as usize,
                buf_space.lock().len() * core::mem::size_of::<u32>()
            );

            // 创建双缓冲区
            let buffer = Self {
                width: device_buffer_guard.width,
                height: device_buffer_guard.height,
                size: device_buffer_guard.size,
                bit_depth: device_buffer_guard.bit_depth,
                flags: buf_type,
                buf: ScmBuffer::DoubleBuffer(buf_space),
            };
            drop(device_buffer_guard);

            return Ok(buffer);
        }
    }

    pub unsafe fn new_device_buffer(
        width: u32,
        height: u32,
        size: u32,
        bit_depth: u32,
        buf_type: ScmBufferFlag,
        vaddr: VirtAddr,
    ) -> Result<Self, SystemError> {
        let buffer = Self {
            width,
            height,
            size,
            bit_depth,
            flags: buf_type,
            buf: ScmBuffer::DeviceBuffer(vaddr),
        };
        return Ok(buffer);
    }

    pub fn buf_size(&self) -> usize {
        self.size as usize
    }

    pub fn bit_depth(&self) -> u32 {
        self.bit_depth
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn is_double_buffer(&self) -> bool {
        match &self.buf {
            ScmBuffer::DoubleBuffer(_) => true,
            _ => false,
        }
    }
    pub fn is_device_buffer(&self) -> bool {
        match &self.buf {
            ScmBuffer::DeviceBuffer(_) => true,
            _ => false,
        }
    }

    pub fn copy_from_nonoverlapping(&mut self, src: &ScmBufferInfo) {
        assert!(self.buf_size() == src.buf_size());
        match &self.buf {
            ScmBuffer::DeviceBuffer(vaddr) => {
                let len = self.buf_size() / core::mem::size_of::<u32>();
                let self_buf_guard =
                    unsafe { core::slice::from_raw_parts_mut(vaddr.data() as *mut u32, len) };
                match &src.buf {
                    ScmBuffer::DeviceBuffer(vaddr) => {
                        let src_buf_guard =
                            unsafe { core::slice::from_raw_parts(vaddr.data() as *const u32, len) };
                        self_buf_guard.copy_from_slice(src_buf_guard);
                    }
                    ScmBuffer::DoubleBuffer(double_buffer) => {
                        let src_buf_guard = double_buffer.lock();
                        self_buf_guard.copy_from_slice(src_buf_guard.as_ref());
                    }
                };
            }

            ScmBuffer::DoubleBuffer(double_buffer) => {
                let mut double_buffer_guard = double_buffer.lock();
                match &src.buf {
                    ScmBuffer::DeviceBuffer(vaddr) => {
                        let len = src.buf_size() / core::mem::size_of::<u32>();
                        double_buffer_guard.as_mut().copy_from_slice(unsafe {
                            core::slice::from_raw_parts(vaddr.data() as *const u32, len)
                        });
                    }
                    ScmBuffer::DoubleBuffer(double_buffer) => {
                        let x = double_buffer.lock();
                        double_buffer_guard.as_mut().copy_from_slice(x.as_ref());
                    }
                };
            }
        }
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, PartialOrd, Ord, Hash)]
pub struct ScmUiFrameworkId(u32);

impl ScmUiFrameworkId {
    /// 分配一个新的框架id
    pub fn new() -> Self {
        static MAX_ID: AtomicU32 = AtomicU32::new(0);
        return ScmUiFrameworkId(MAX_ID.fetch_add(1, Ordering::SeqCst));
    }
}
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ScmUiFrameworkMetadata {
    id: ScmUiFrameworkId,
    name: String,
    framework_type: ScmFramworkType,
    pub buf_info: ScmBufferInfo,
}

impl ScmUiFrameworkMetadata {
    pub fn new(name: String, framework_type: ScmFramworkType) -> Self {
        match framework_type {
            ScmFramworkType::Text => {
                let result = ScmUiFrameworkMetadata {
                    id: ScmUiFrameworkId::new(),
                    name,
                    framework_type: ScmFramworkType::Text,
                    buf_info: ScmBufferInfo::new(
                        ScmBufferFlag::SCM_BF_TEXT | ScmBufferFlag::SCM_BF_DB,
                    )
                    .unwrap(),
                };

                return result;
            }
            ScmFramworkType::Gui => todo!(),
            ScmFramworkType::Unused => todo!(),
        }
    }
    pub fn buf_info(&self) -> ScmBufferInfo {
        return self.buf_info.clone();
    }
    pub fn set_buf_info(&mut self, buf_info: ScmBufferInfo) {
        self.buf_info = buf_info;
    }
}
pub trait ScmUiFramework: Sync + Send + Debug {
    // 安装ui框架的回调函数
    fn install(&self) -> Result<i32, SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    // 卸载ui框架的回调函数
    fn uninstall(&self) -> Result<i32, SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    // 启用ui框架的回调函数
    fn enable(&self) -> Result<i32, SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    // 禁用ui框架的回调函数
    fn disable(&self) -> Result<i32, SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    // 改变ui框架的帧缓冲区的回调函数
    fn change(&self, _buf: ScmBufferInfo) -> Result<i32, SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    /// @brief 获取ScmUiFramework的元数据
    /// @return 成功：Ok(ScmUiFramework的元数据)
    ///         失败：Err(错误码)
    fn metadata(&self) -> Result<ScmUiFrameworkMetadata, SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
}

/// 初始化屏幕控制模块
///
/// ## 调用时机
///
/// 该函数在内核启动的早期进行调用。调用时，内存管理模块尚未初始化。
pub fn scm_init(enable_put_to_window: bool) {
    SCM_DOUBLE_BUFFER_ENABLED.store(false, Ordering::SeqCst); // 禁用双缓冲
    if enable_put_to_window {
        textui_enable_put_to_window();
    } else {
        textui_disable_put_to_window();
    }
    textui_init_no_alloc(enable_put_to_window);

    send_to_default_serial8250_port("\nfinish_scm_init\n\0".as_bytes());
}

/// 启用某个ui框架，将它的帧缓冲区渲染到屏幕上
/// ## 参数
///
/// - framework 要启动的ui框架

pub fn scm_framework_enable(framework: Arc<dyn ScmUiFramework>) -> Result<i32, SystemError> {
    // 获取信息
    let metadata = framework.metadata()?;

    // if metadata.buf_info.buf.is_null() {
    //     return Err(SystemError::EINVAL);
    // }
    let mut current_framework = CURRENT_FRAMEWORK.write();

    if SCM_DOUBLE_BUFFER_ENABLED.load(Ordering::SeqCst) == true {
        video_refresh_manager().set_refresh_target(&metadata.buf_info)?;
    }

    framework.enable()?;
    current_framework.replace(framework);

    return Ok(0);
}
/// 向屏幕管理器注册UI框架
///
/// ## 参数
/// - framework 框架结构体

pub fn scm_register(framework: Arc<dyn ScmUiFramework>) -> Result<i32, SystemError> {
    // 把ui框架加入链表

    SCM_FRAMEWORK_LIST.lock().push_back(framework.clone());
    // 调用ui框架的回调函数以安装ui框架，并将其激活
    framework.install()?;

    // 如果当前还没有框架获得了屏幕的控制权，就让其拿去
    if CURRENT_FRAMEWORK.read().is_none() {
        return scm_framework_enable(framework);
    }
    return Ok(0);
}

/// 屏幕管理器启用双缓冲区
#[allow(dead_code)]
pub fn scm_enable_double_buffer() -> Result<i32, SystemError> {
    if SCM_DOUBLE_BUFFER_ENABLED.load(Ordering::SeqCst) {
        // 已经开启了双缓冲区了, 直接退出
        return Ok(0);
    }
    let scm_list = SCM_FRAMEWORK_LIST.lock();
    if scm_list.is_empty() {
        // scm 框架链表为空
        return Ok(0);
    }
    drop(scm_list);
    SCM_DOUBLE_BUFFER_ENABLED.store(true, Ordering::SeqCst);
    // 创建双缓冲区
    let buf_info = ScmBufferInfo::new(ScmBufferFlag::SCM_BF_DB | ScmBufferFlag::SCM_BF_PIXEL)?;

    // 设置定时刷新的对象
    video_refresh_manager()
        .set_refresh_target(&buf_info)
        .expect("set refresh target failed");

    // 设置当前框架的帧缓冲区
    CURRENT_FRAMEWORK
        .write()
        .as_ref()
        .unwrap()
        .change(buf_info)?;
    // 遍历当前所有使用帧缓冲区的框架，更新为双缓冲区
    for framework in SCM_FRAMEWORK_LIST.lock().iter_mut() {
        if !(*framework).metadata()?.buf_info.is_double_buffer() {
            let new_buf_info =
                ScmBufferInfo::new(ScmBufferFlag::SCM_BF_DB | ScmBufferFlag::SCM_BF_PIXEL)?;
            (*framework).change(new_buf_info)?;
        }
    }
    // 通知显示驱动，启动双缓冲
    video_refresh_manager().video_reinitialize(true)?;

    return Ok(0);
}

/// 允许往窗口打印信息
pub fn scm_enable_put_to_window() {
    // mm之前要继续往窗口打印信息时，因为没有动态内存分配(textui并没有往scm注册)，且使用的是textui,要直接修改textui里面的值
    if CURRENT_FRAMEWORK.read().is_none() {
        textui_enable_put_to_window();
    } else {
        let r = CURRENT_FRAMEWORK
            .write()
            .as_ref()
            .unwrap()
            .enable()
            .unwrap_or_else(|e| e.to_posix_errno());
        if r.is_negative() {
            send_to_default_serial8250_port("scm_enable_put_to_window() failed.\n\0".as_bytes());
        }
    }
}
/// 禁止往窗口打印信息
pub fn scm_disable_put_to_window() {
    // mm之前要停止往窗口打印信息时，因为没有动态内存分配(rwlock与otion依然能用，但是textui并没有往scm注册)，且使用的是textui,要直接修改textui里面的值
    if CURRENT_FRAMEWORK.read().is_none() {
        textui_disable_put_to_window();
        assert!(textui_is_enable_put_to_window() == false);
    } else {
        let r = CURRENT_FRAMEWORK
            .write()
            .as_ref()
            .unwrap()
            .disable()
            .unwrap_or_else(|e| e.to_posix_errno());
        if r.is_negative() {
            send_to_default_serial8250_port("scm_disable_put_to_window() failed.\n\0".as_bytes());
        }
    }
}
/// 当内存管理单元被初始化之后，重新处理帧缓冲区问题
#[inline(never)]
pub fn scm_reinit() -> Result<(), SystemError> {
    #[cfg(target_arch = "x86_64")]
    {
        let r = true_scm_reinit();
        if r.is_err() {
            send_to_default_serial8250_port("scm reinit failed.\n\0".as_bytes());
        }
        return r;
    }

    #[cfg(not(target_arch = "x86_64"))]
    {
        return Ok(());
    }
}

#[allow(dead_code)]
fn true_scm_reinit() -> Result<(), SystemError> {
    video_refresh_manager()
        .video_reinitialize(false)
        .expect("video reinitialize failed");

    // 遍历当前所有使用帧缓冲区的框架，更新地址
    let device_buffer = video_refresh_manager().device_buffer().clone();
    for framework in SCM_FRAMEWORK_LIST.lock().iter_mut() {
        if framework.metadata()?.buf_info().is_device_buffer() {
            framework.change(device_buffer.clone())?;
        }
    }

    scm_enable_put_to_window();

    return Ok(());
}
