use core::{
    fmt::Debug,
    intrinsics::unlikely,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use alloc::{boxed::Box, collections::LinkedList, string::String, sync::Arc};

use crate::{
    driver::uart::uart_device::{c_uart_send_str, UartPort},
    include::bindings::bindings::{
        scm_buffer_info_t, video_frame_buffer_info, video_reinitialize, video_set_refresh_target,
    },
    libs::{rwlock::RwLock, spinlock::SpinLock},
    mm::VirtAddr,
    syscall::SystemError,
};

use lazy_static::lazy_static;

use super::textui_no_alloc::textui_init_no_alloc;

lazy_static! {
    /// 全局的UI框架列表
    pub static ref SCM_FRAMEWORK_LIST: SpinLock<LinkedList<Arc<dyn ScmUiFramework>>> =
        SpinLock::new(LinkedList::new());
    /// 当前在使用的UI框架
    pub static ref CURRENT_FRAMEWORK: RwLock<Option<Arc<dyn ScmUiFramework>>> = RwLock::new(None);

}

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
#[derive(Debug)]
pub enum ScmBuffer {
    DeviceBuffer(Option<VirtAddr>),
    DoubleBuffer(Option<Box<[u32]>>),
}
#[derive(Debug)]
pub struct ScmBufferInfo {
    width: u32,     // 帧缓冲区宽度（pixel或columns）
    height: u32,    // 帧缓冲区高度（pixel或lines）
    size: u32,      // 帧缓冲区大小（bytes）
    bit_depth: u32, // 像素点位深度
    pub buf: ScmBuffer,
    flags: ScmBufferFlag, // 帧缓冲区标志位
}
impl Clone for ScmBufferInfo {
    fn clone(&self) -> Self {
        match self.buf {
            ScmBuffer::DeviceBuffer(_) => ScmBufferInfo {
                width: self.width,
                height: self.height,
                size: self.size,
                bit_depth: self.bit_depth,
                flags: self.flags,
                buf: ScmBuffer::DeviceBuffer(Option::None),
            },
            ScmBuffer::DoubleBuffer(_) => ScmBufferInfo {
                width: self.width,
                height: self.height,
                size: self.size,
                bit_depth: self.bit_depth,
                flags: self.flags,
                buf: ScmBuffer::DoubleBuffer(Option::None),
            },
        }
    }
}

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
    pub fn new(buf_type: ScmBufferFlag) -> Result<Self, SystemError> {
        if unlikely(SCM_DOUBLE_BUFFER_ENABLED.load(Ordering::SeqCst) == false) {
            let buf_info = ScmBufferInfo::from(unsafe { &video_frame_buffer_info });

            return Ok(buf_info);
        } else {
            // 创建双缓冲区
            let mut frame_buffer_info: ScmBufferInfo =
                ScmBufferInfo::from(unsafe { &video_frame_buffer_info });

            frame_buffer_info.flags = buf_type;
            // 这里还是改成使用box来存储数组，如果直接用vec存储，在multiboot2_iter那里会报错，不知为何
            frame_buffer_info.buf = ScmBuffer::DoubleBuffer(Some(
                Box::new(vec![
                    0;
                    unsafe { (video_frame_buffer_info.size / 4) as usize }
                ])
                .into_boxed_slice(),
            ));

            return Ok(frame_buffer_info);
        }
    }

    // 重构了video后可以删除
    fn vaddr(&mut self) -> VirtAddr {
        match &self.buf {
            ScmBuffer::DeviceBuffer(vaddr) => {
                if !vaddr.is_none() {
                    vaddr.unwrap()
                } else {
                    return VirtAddr::new(0);
                }
            }
            ScmBuffer::DoubleBuffer(buf) => {
                if !buf.is_none() {
                    let address = self.buf().as_ptr();
                    VirtAddr::new(address as usize)
                } else {
                    return VirtAddr::new(0);
                }
            }
        }
    }

    fn buf(&mut self) -> &mut [u32] {
        let len = self.buf_size() / 4;
        match &mut self.buf {
            ScmBuffer::DoubleBuffer(buf) => match buf.as_mut() {
                Some(buf) => buf,
                None => panic!("Buffer is none"),
            },
            ScmBuffer::DeviceBuffer(vaddr) => match vaddr.as_mut() {
                Some(vaddr) => {
                    let buf: &mut [u32] = unsafe {
                        core::slice::from_raw_parts_mut(vaddr.data() as *mut u32, len as usize)
                    };
                    return buf;
                }
                None => panic!("Buffer is none"),
            },
        }
    }
    pub fn buf_size(&self) -> u32 {
        self.size
    }
    pub fn buf_height(&self) -> u32 {
        self.height
    }
    pub fn buf_width(&self) -> u32 {
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
}

// 重构了video后可以删除
impl From<&scm_buffer_info_t> for ScmBufferInfo {
    fn from(value: &scm_buffer_info_t) -> Self {
        Self {
            width: value.width,
            height: value.height,
            size: value.size,
            bit_depth: value.bit_depth,
            buf: ScmBuffer::DeviceBuffer(Some(VirtAddr::new(value.vaddr as usize))),
            flags: ScmBufferFlag::from_bits_truncate(value.flags as u8),
        }
    }
}
impl Into<scm_buffer_info_t> for ScmBufferInfo {
    fn into(mut self) -> scm_buffer_info_t {
        let vaddr = self.vaddr();
        scm_buffer_info_t {
            width: self.width,
            height: self.height,
            size: self.size,
            bit_depth: self.bit_depth,
            vaddr: vaddr.data() as u64,
            flags: self.flags.bits as u64,
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
                    buf_info: ScmBufferInfo::new(ScmBufferFlag::SCM_BF_TEXT).unwrap(),
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
    pub fn buf_is_none(&self) -> bool {
        match &self.buf_info.buf {
            ScmBuffer::DeviceBuffer(vaddr) => {
                return vaddr.is_none();
            }
            ScmBuffer::DoubleBuffer(buf) => {
                return buf.is_none();
            }
        }
    }
    pub fn buf(&mut self) -> &mut [u32] {
        if self.buf_is_none() {
            panic!("buf is none");
        }
        self.buf_info.buf()
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
#[no_mangle]
pub extern "C" fn scm_init() {
    SCM_DOUBLE_BUFFER_ENABLED.store(false, Ordering::SeqCst); // 禁用双缓冲

    textui_init_no_alloc();

    c_uart_send_str(UartPort::COM1.to_u16(), "\nfinish_scm_init\n\0".as_ptr());
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
        let buf: scm_buffer_info_t = metadata.buf_info.into();
        let retval = unsafe { video_set_refresh_target(buf) };
        if retval == 0 {
            framework.enable()?;
        }
    } else {
        framework.enable()?;
    }

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

/// 允许双缓冲区
#[no_mangle]
pub extern "C" fn scm_enable_double_buffer() -> i32 {
    let r = true_scm_enable_double_buffer().unwrap_or_else(|e| e.to_posix_errno());
    if r.is_negative() {
        c_uart_send_str(
            UartPort::COM1.to_u16(),
            "scm enable double buffer fail.\n\0".as_ptr(),
        );
    }

    return r;
}
fn true_scm_enable_double_buffer() -> Result<i32, SystemError> {
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
    let mut buf_info = ScmBufferInfo::new(ScmBufferFlag::SCM_BF_DB | ScmBufferFlag::SCM_BF_PIXEL)?;
    let mut refresh_target_buf: scm_buffer_info_t = buf_info.clone().into();
    // 重构video后进行修改
    refresh_target_buf.vaddr = buf_info.vaddr().data() as u64;
    CURRENT_FRAMEWORK
        .write()
        .as_ref()
        .unwrap()
        .change(buf_info)?;
    // 设置定时刷新的对象
    unsafe { video_set_refresh_target(refresh_target_buf) };
    // 遍历当前所有使用帧缓冲区的框架，更新为双缓冲区
    for framework in SCM_FRAMEWORK_LIST.lock().iter_mut() {
        if !(*framework).metadata()?.buf_info.is_double_buffer() {
            let new_buf_info =
                ScmBufferInfo::new(ScmBufferFlag::SCM_BF_DB | ScmBufferFlag::SCM_BF_PIXEL)?;
            (*framework).change(new_buf_info)?;
        }
    }
    // 通知显示驱动，启动双缓冲
    unsafe { video_reinitialize(true) };

    return Ok(0);
}
/// 允许往窗口打印信息
#[no_mangle]
pub fn scm_enable_put_to_window() {
    // mm之前要继续往窗口打印信息时，因为没有动态内存分配(rwlock与otion依然能用，但是textui并没有往scm注册)，且使用的是textui,要直接修改textui里面的值
    if CURRENT_FRAMEWORK.read().is_none() {
        super::textui::ENABLE_PUT_TO_WINDOW.store(true, Ordering::SeqCst);
    } else {
        let r = CURRENT_FRAMEWORK
            .write()
            .as_ref()
            .unwrap()
            .enable()
            .unwrap_or_else(|e| e.to_posix_errno());
        if r.is_negative() {
            c_uart_send_str(
                UartPort::COM1.to_u16(),
                "scm_enable_put_to_window() failed.\n\0".as_ptr(),
            );
        }
    }
}
/// 禁止往窗口打印信息
#[no_mangle]
pub fn scm_disable_put_to_window() {
    // mm之前要停止往窗口打印信息时，因为没有动态内存分配(rwlock与otion依然能用，但是textui并没有往scm注册)，且使用的是textui,要直接修改textui里面的值
    if CURRENT_FRAMEWORK.read().is_none() {
        super::textui::ENABLE_PUT_TO_WINDOW.store(false, Ordering::SeqCst);
        assert!(super::textui::ENABLE_PUT_TO_WINDOW.load(Ordering::SeqCst) == false);
    } else {
        let r = CURRENT_FRAMEWORK
            .write()
            .as_ref()
            .unwrap()
            .disable()
            .unwrap_or_else(|e| e.to_posix_errno());
        if r.is_negative() {
            c_uart_send_str(
                UartPort::COM1.to_u16(),
                "scm_disable_put_to_window() failed.\n\0".as_ptr(),
            );
        }
    }
}
/// 当内存管理单元被初始化之后，重新处理帧缓冲区问题
#[no_mangle]
pub extern "C" fn scm_reinit() -> i32 {
    let r = true_scm_reinit().unwrap_or_else(|e| e.to_posix_errno());
    if r.is_negative() {
        c_uart_send_str(UartPort::COM1.to_u16(), "scm reinit failed.\n\0".as_ptr());
    }
    return r;
}
fn true_scm_reinit() -> Result<i32, SystemError> {
    unsafe { video_reinitialize(false) };

    // 遍历当前所有使用帧缓冲区的框架，更新地址
    for framework in SCM_FRAMEWORK_LIST.lock().iter_mut() {
        if framework.metadata()?.buf_info().is_device_buffer() {
            framework.change(unsafe { ScmBufferInfo::from(&video_frame_buffer_info) })?;
        }
    }

    scm_enable_put_to_window();

    return Ok(0);
}
