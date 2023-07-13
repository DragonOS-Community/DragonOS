use core::{
    fmt::Debug,
    intrinsics::unlikely,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use alloc::{
    collections::LinkedList,
    string::{String, ToString},
    sync::Arc,
};

use crate::{
    driver::uart::uart::{c_uart_send_str, UartPort},
    include::bindings::bindings::{
        alloc_pages, scm_buffer_info_t, video_frame_buffer_info, video_reinitialize,
        video_set_refresh_target, Page, PAGE_2M_SIZE, ZONE_NORMAL,
    },
    libs::{rwlock::RwLock, spinlock::SpinLock},
    mm::{phys_2_virt, PAGE_2M_ALIGN},
    syscall::SystemError,
};

use super::{
    textui::{renew_buf, textui_change_buf},
    textui_no_alloc::textui_init_no_alloc,
};

use lazy_static::lazy_static;

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
  pub struct ScmBfFlag:u8{
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
pub struct ScmBufferInfo {
    width: u32,       // 帧缓冲区宽度（pixel或columns）
    height: u32,      // 帧缓冲区高度（pixel或lines）
    size: u32,        // 帧缓冲区大小（bytes）
    bit_depth: u32,   // 像素点位深度
    vaddr: usize,     // 指向帧缓冲区的指针(用于video里面的scm_buffer_info_t)
    flags: ScmBfFlag, // 帧缓冲区标志位
}

fn alloc_pages_for_video_frame_buffer_info_size() -> Result<*mut Page, SystemError> {
    let p: *mut Page = unsafe {
        alloc_pages(
            ZONE_NORMAL,
            (PAGE_2M_ALIGN(video_frame_buffer_info.size) / PAGE_2M_SIZE) as i32,
            0,
        )
    };
    if p.is_null() {
        return Err(SystemError::ENOMEM);
    }
    return Ok(p);
}

fn get_vaddr_of_double_buf() -> Result<usize, SystemError> {
    let p = alloc_pages_for_video_frame_buffer_info_size()?;
    let vaddr = phys_2_virt(((unsafe { *p }).addr_phys) as usize);
    return Ok(vaddr);
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
    pub fn new(buf_type: ScmBfFlag) -> Result<Self, SystemError> {
        if unlikely(SCM_DOUBLE_BUFFER_ENABLED.load(Ordering::SeqCst) == false) {
            return Ok(ScmBufferInfo::from(unsafe { &video_frame_buffer_info }));
        } else {
            //创建双缓冲区
            let mut frame_buffer_info: ScmBufferInfo =
                ScmBufferInfo::from(unsafe { &video_frame_buffer_info });
            frame_buffer_info.flags = buf_type;

            frame_buffer_info.vaddr = get_vaddr_of_double_buf()?;
            // println!("vaddr:{}", frame_buffer_info.vaddr);

            return Ok(frame_buffer_info);
        }
    }

    pub fn get_vaddr(&self) -> usize {
        self.vaddr
    }
    pub fn get_size_about_u8(&self) -> u32 {
        self.size
    }
    pub fn get_height_about_u32(&self) -> u32 {
        self.height
    }
    pub fn get_width_about_u32(&self) -> u32 {
        self.width
    }
    pub fn get_size_about_u32(&self) -> u32 {
        self.height * self.width
    }
}

impl From<&scm_buffer_info_t> for ScmBufferInfo {
    fn from(value: &scm_buffer_info_t) -> Self {
        Self {
            width: value.width,
            height: value.height,
            size: value.size,
            bit_depth: value.bit_depth,
            vaddr: value.vaddr as usize,
            flags: ScmBfFlag::from_bits_truncate(value.flags as u8),
        }
    }
}
impl Into<scm_buffer_info_t> for ScmBufferInfo {
    fn into(self) -> scm_buffer_info_t {
        scm_buffer_info_t {
            width: self.width,
            height: self.height,
            size: self.size,
            bit_depth: self.bit_depth,
            vaddr: self.vaddr as u64,
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

#[derive(Debug, Clone)]
pub struct ScmUiFrameworkMetadata {
    pub id: ScmUiFrameworkId,
    pub name: String,
    pub framework_type: ScmFramworkType,
    pub buf_info: ScmBufferInfo,
    // pub private_info: ScmUiPrivateInfo,
    pub is_enable: bool,
}

impl ScmUiFrameworkMetadata {
    pub fn new(name: String, framework_type: ScmFramworkType) -> Self {
        match framework_type {
            ScmFramworkType::Text => {
                let result = ScmUiFrameworkMetadata {
                    id: ScmUiFrameworkId::new(),
                    name: "".to_string(),
                    framework_type: ScmFramworkType::Text,
                    buf_info: ScmBufferInfo::new(ScmBfFlag::SCM_BF_TEXT).unwrap(),
                    is_enable: false,
                };
                return result;
            }
            ScmFramworkType::Gui => todo!(),
            ScmFramworkType::Unused => todo!(),
        }
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

/**
 * @brief 启用某个ui框架，将它的帧缓冲区渲染到屏幕上
 *
 * @param framework 要启动的ui框架
 */
pub fn scm_framework_enable(framework: Arc<dyn ScmUiFramework>) -> Result<i32, SystemError> {
    if framework.metadata()?.buf_info.vaddr == 0 {
        return Err(SystemError::EINVAL);
    }
    let mut current_framework = CURRENT_FRAMEWORK.write();
    // 获取信息
    let metadata = framework.metadata()?;

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
/**
 * @brief 向屏幕管理器注册UI框架
 *
 * @param framework 框架结构体
 */
pub fn scm_register(framework: Arc<dyn ScmUiFramework>) -> Result<i32, SystemError> {
    // 把ui框架加入链表

    SCM_FRAMEWORK_LIST.lock().push_back(framework.clone());
    // 调用ui框架的回调函数以安装ui框架，并将其激活
    framework.install()?;

    // 如果当前还没有框架获得了屏幕的控制权，就让其拿去
    if !CURRENT_FRAMEWORK.read().is_none() {
        return scm_framework_enable(framework);
    }

    return Ok(0);
}

/**
 * @brief 允许双缓冲区
 *
 * @return int
 */
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
    // let mut scm_list = SCM_FRAMEWORK_LIST.lock();
    // if scm_list.is_empty() {
    //     // scm 框架链表为空
    //     return Ok(0);
    // }
    SCM_DOUBLE_BUFFER_ENABLED.store(true, Ordering::SeqCst);

    // 创建双缓冲区
    let buf_info = ScmBufferInfo::new(ScmBfFlag::SCM_BF_DB | ScmBfFlag::SCM_BF_PIXEL)?;

    CURRENT_FRAMEWORK
        .write()
        .as_ref()
        .unwrap()
        .change(buf_info.clone())?;
    // 设置定时刷新的对象
    unsafe { video_set_refresh_target(buf_info.clone().into()) };
    textui_change_buf(buf_info.clone())?;
    // 通知显示驱动，启动双缓冲
    unsafe { video_reinitialize(true) };
    renew_buf(buf_info.get_vaddr(), buf_info.get_height_about_u32());
    // println!("vaddr:{:#018x}",buf_info.get_vaddr());
    // loop{}
    return Ok(0);
}

/**
 * @brief 当内存管理单元被初始化之后，重新处理帧缓冲区问题
 *
 */
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
    // 遍历当前所有使用帧缓冲区的框架，更新地址(暂时还没想好怎么不用指针把各个框架的缓冲区更改为双缓冲区，先直接把textui框架的缓冲区更改为双缓冲)
    // for framework in SCM_FRAMEWORK_LIST.lock().iter_mut() {
    //     if unsafe {
    //         scm_buffer_info_is_equal(
    //             framework.metadata()?.buf_info.into(),
    //             video_frame_buffer_info,
    //         )
    //     } {
    //         framework.change(unsafe { ScmBufferInfo::from(&video_frame_buffer_info) })?;
    //     }
    // }
    textui_change_buf(unsafe { ScmBufferInfo::from(&video_frame_buffer_info) })?;
    // unsafe { scm_enable_put_to_window() };
    return Ok(0);
}
