use core::{
    fmt::Debug,
    intrinsics::unlikely,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use alloc::{
    boxed::Box,
    collections::LinkedList,
    string::{String, ToString},
    sync::Arc,
};

use crate::{
    driver::uart::uart::{c_uart_send_str, UartPort},
    include::bindings::bindings::{
        alloc_pages, buffer_equal, free_pages, scm_buffer_info_t, verify_area,
        video_frame_buffer_info, video_reinitialize, video_set_refresh_target, Page, PAGE_2M_SIZE,
        ZONE_NORMAL,
    },
    libs::spinlock::SpinLock,
    mm::{phys_2_virt, virt_2_phys, Phy_to_2M_Page, PAGE_2M_ALIGN},
    syscall::SystemError,
};

use super::textui::TextuiPrivateInfo;

use lazy_static::lazy_static;
lazy_static! {
    pub static ref SCM_FRAMEWORK_LIST: SpinLock<LinkedList<Arc<dyn ScmUiFramework>>> =
        SpinLock::new(LinkedList::new());
}
lazy_static! {
    pub static ref CURRENT_FRAMEWORK_METADATA: SpinLock<ScmUiFrameworkMetadata> =
        SpinLock::new(ScmUiFrameworkMetadata::new(ScmFramworkType::Text));
}
pub static mut SCM_DOUBLE_BUFFER_ENABLED: AtomicBool = AtomicBool::new(false); // 允许双缓冲的标志位

bitflags! {
  pub struct ScmBfFlag:u8{
    // 帧缓冲区标志位
       const SCM_BF_FB= 1 << 0; // 当前buffer是设备显存中的帧缓冲区
       const SCM_BF_DB= 1 << 1; // 当前buffer是双缓冲
       const SCM_BF_TEXT= 1 << 2; // 使用文本模式
       const SCM_BF_PIXEL= 1 << 3; // 使用图像模式
   }
}

#[derive(Clone, Debug)]
pub enum ScmUiPrivateInfo {
    Textui(TextuiPrivateInfo),
    Gui,
    Unused,
}
#[derive(Clone, Debug)]
pub enum ScmFramworkType {
    Text,
    Gui,
    Unused,
}

#[derive(Clone, Debug)]
pub struct ScmBufferInfo {
    pub width: u32,     // 帧缓冲区宽度（pixel或columns）
    pub height: u32,    // 帧缓冲区高度（pixel或lines）
    pub size: u32,      // 帧缓冲区大小（bytes）
    pub bit_depth: u32, // 像素点位深度

    pub vaddr: u64,       // 帧缓冲区的地址
    pub flags: ScmBfFlag, // 帧缓冲区标志位
}
impl ScmBufferInfo {
    /**
     * @brief 创建新的帧缓冲区
     *
     * @param b_type 帧缓冲区类型
     * @return struct ScmBufferInfo 新的帧缓冲区结构体
     */
    fn new(b_type: ScmBfFlag) -> Result<Self, SystemError> {
        c_uart_send_str(UartPort::COM1.to_u16(), "\nscm_buf_new_start\n\0".as_ptr());
        if unlikely(unsafe { SCM_DOUBLE_BUFFER_ENABLED.load(Ordering::SeqCst) } == false) {
            c_uart_send_str(UartPort::COM1.to_u16(), "\nscm_buf_new_4\n\0".as_ptr());
            return Ok(ScmBufferInfo::copy_from_c(unsafe { &video_frame_buffer_info }).unwrap());
        } else {
            let mut frame_buffer_info: ScmBufferInfo = ScmBufferInfo {
                width: unsafe { video_frame_buffer_info.width }, // 帧缓冲区宽度（pixel或columns）
                height: unsafe { video_frame_buffer_info.height }, // 帧缓冲区高度（pixel或lines）
                size: unsafe { video_frame_buffer_info.size },   // 帧缓冲区大小（bytes）
                bit_depth: unsafe { video_frame_buffer_info.bit_depth }, // 像素点位深度

                vaddr: 0,                    // 帧缓冲区的地址
                flags: ScmBfFlag::SCM_BF_DB, // 帧缓冲区标志位
            };
            if b_type.contains(ScmBfFlag::SCM_BF_PIXEL) {
                frame_buffer_info.flags |= ScmBfFlag::SCM_BF_PIXEL;
            } else {
                frame_buffer_info.flags |= ScmBfFlag::SCM_BF_TEXT;
            }

            let p: *mut Page = unsafe {
                alloc_pages(
                    ZONE_NORMAL,
                    (PAGE_2M_ALIGN(video_frame_buffer_info.size) / PAGE_2M_SIZE) as i32,
                    0,
                )
            };
            c_uart_send_str(UartPort::COM1.to_u16(), "\nscm_buf_new_2\n\0".as_ptr());
            if p.is_null() {
                c_uart_send_str(UartPort::COM1.to_u16(), "\nscm_buf_new_1\n\0".as_ptr());
                return Err(SystemError::ENOMEM);
            } else {
                frame_buffer_info.vaddr = phys_2_virt(((unsafe { *p }).addr_phys) as usize) as u64;
                c_uart_send_str(UartPort::COM1.to_u16(), "\nscm_buf_new_finish\n\0".as_ptr());
                return Ok(frame_buffer_info);
            }
        }
    }
    pub fn to_c(&self) -> scm_buffer_info_t {
        scm_buffer_info_t {
            width: self.width,
            height: self.height,
            size: self.size,
            bit_depth: self.bit_depth,
            vaddr: self.vaddr,
            flags: self.flags.bits as u64,
        }
    }
    fn copy_from_c(buf: &scm_buffer_info_t) -> Result<Self, SystemError> {
        Ok(ScmBufferInfo {
            width: buf.width,
            height: buf.height,
            size: buf.size,
            bit_depth: buf.bit_depth,
            vaddr: buf.vaddr,
            flags: ScmBfFlag::from_bits_truncate(buf.flags as u8),
        })
    }
}
#[derive(Clone, Debug)]
pub struct ScmUiFrameworkMetadata {
    // pub list: LinkedList<Box<TextuiWindow>>,
    pub id: i16,
    pub name: String,
    pub f_type: ScmFramworkType,
    pub buf: ScmBufferInfo,
    // pub private_info: ScmUiPrivateInfo,
    pub is_null: bool,
    pub window_max_id: u32,
}
static F_ID: AtomicUsize = AtomicUsize::new(0);

impl ScmUiFrameworkMetadata {
    pub fn new(f_type: ScmFramworkType) -> Self {
        match f_type {
            ScmFramworkType::Text => {
                let count = F_ID.fetch_add(1, Ordering::SeqCst);
                let result = ScmUiFrameworkMetadata {
                    // list: LinkedList::new(),
                    id: count as i16,
                    name: "".to_string(),
                    f_type: ScmFramworkType::Text,
                    buf: ScmBufferInfo::new(ScmBfFlag::SCM_BF_TEXT).unwrap(),
                    // private_info: ScmUiPrivateInfo::Textui(TextuiPrivateInfo::new()),
                    is_null: true,
                    window_max_id: 0,
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
    fn install(&self, _buf: ScmBufferInfo) -> Result<i32, SystemError> {
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
    ///
    /// @return 成功：Ok(ScmUiFramework的元数据)
    ///         失败：Err(错误码)
    fn metadata(&self) -> Result<ScmUiFrameworkMetadata, SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    // fn clone_box(&self) -> Result<Box<dyn ScmUiFramework>,SystemError>{
    //     return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    // }
}

/**
 * @brief 初始化屏幕管理模块
 *
 */
#[no_mangle]
pub extern "C" fn scm_init() {
    // spin_init(&scm_register_lock);
    // spin_init(&scm_screen_own_lock);
    //  io_mfence();
    // fence(Ordering::SeqCst);
    c_uart_send_str(UartPort::COM1.to_u16(), "\nscm_init\n\0".as_ptr());
    unsafe { SCM_DOUBLE_BUFFER_ENABLED.store(false, Ordering::SeqCst) }; // 禁用双缓冲

    // CURRENT_FRAMEWORK_METADATA.lock().is_null = true;
    c_uart_send_str(UartPort::COM1.to_u16(), "\nfinish_init\n\0".as_ptr());
}
/**
 * @brief 启用某个ui框架，将它的帧缓冲区渲染到屏幕上
 *
 * @param ui 要启动的ui框架
 * @return int 返回码
 */
pub fn scm_framework_enable(textuiframework: Arc<dyn ScmUiFramework>) -> Result<i32, SystemError> {
    if textuiframework.metadata()?.buf.vaddr == 0 {
        return Err(SystemError::EINVAL);
    }
    let mut current_framework = CURRENT_FRAMEWORK_METADATA.lock();

    // spin_lock(&scm_screen_own_lock);

    if unsafe { SCM_DOUBLE_BUFFER_ENABLED.load(Ordering::SeqCst) } == true {
        let buf: *mut scm_buffer_info_t =
            &mut textuiframework.metadata()?.buf.to_c() as *mut scm_buffer_info_t;
        let retval = unsafe { video_set_refresh_target(buf) };
        if retval == 0 {
            current_framework.id = textuiframework.metadata()?.id;
            current_framework.buf = textuiframework.metadata()?.buf;
            current_framework.f_type = textuiframework.metadata()?.f_type;
            current_framework.name = textuiframework.metadata()?.name;
            // current_framework.private_info = ui.metadata().unwrap().private_info;
            current_framework.is_null = textuiframework.metadata()?.is_null;
            current_framework.window_max_id = textuiframework.metadata()?.window_max_id;
        }
    } else {
        current_framework.id = textuiframework.metadata()?.id;
        current_framework.buf = textuiframework.metadata()?.buf;
        current_framework.f_type = textuiframework.metadata()?.f_type;
        current_framework.name = textuiframework.metadata()?.name;
        // current_framework.private_info = ui.metadata()?.private_info;
        current_framework.is_null = textuiframework.metadata()?.is_null;
        current_framework.window_max_id = textuiframework.metadata()?.window_max_id;
    }

    // spin_unlock(&scm_screen_own_lock);
    return Ok(0);
}
/**
 * @brief 向屏幕管理器注册UI框架（静态设置的框架对象）
 *
 * @param ui 框架结构体指针
 * @return int 错误码
 */
pub fn scm_register(ui: Arc<dyn ScmUiFramework>) -> i32 {
    // 把ui框架加入链表
    SCM_FRAMEWORK_LIST.lock().push_back(ui.clone());
    c_uart_send_str(UartPort::COM1.to_u16(), "\nscm register 1\n\0".as_ptr());
    // 调用ui框架的回调函数以安装ui框架，并将其激活
    let _ = ui.install(ui.metadata().unwrap().buf);
    let _ = ui.enable();
    c_uart_send_str(UartPort::COM1.to_u16(), "\nscm register 2\n\0".as_ptr());

    if CURRENT_FRAMEWORK_METADATA.lock().is_null {
        return scm_framework_enable(ui).unwrap();
    }

    return 0;
}
/**
 * @brief 销毁双缓冲区
 *
 * @param buf
 * @return int
 */
fn destroy_buffer(buf: &mut scm_buffer_info_t) -> Result<i32, SystemError> {
    // 不能销毁帧缓冲区对象
    // if buf == unsafe { &mut video_frame_buffer_info }.as_mut().unwrap() {
    //     return Err(SystemError::EINVAL);
    // }
    if buf.vaddr == 0 {
        return Err(SystemError::EINVAL);
    }
    if unsafe { verify_area(buf.vaddr, buf.size.into()) } {
        return Err(SystemError::EINVAL);
    }
    // 是否双缓冲区
    if ScmBfFlag::from_bits_truncate(buf.flags as u8).contains(ScmBfFlag::SCM_BF_FB) {
        return Err(SystemError::EINVAL);
    }

    // 释放内存页
    let page_size = PAGE_2M_SIZE;
    let page_align = PAGE_2M_ALIGN(unsafe { video_frame_buffer_info.size });
    let page_count = page_align / page_size;
    unsafe {
        free_pages(
            Phy_to_2M_Page(virt_2_phys(buf.vaddr as usize)),
            page_count as i32,
        )
    };

    return Ok(0);
}
/**
 * @brief 允许双缓冲区
 *
 * @return int
 */
#[no_mangle]
pub extern "C" fn scm_enable_double_buffer() -> i32 {
    c_uart_send_str(UartPort::COM1.to_u16(), "\nscm_enable_double\n\0".as_ptr());
    if unsafe { SCM_DOUBLE_BUFFER_ENABLED.load(Ordering::SeqCst) } {
        // 已经开启了双缓冲区了, 直接退出
        return 0;
    }
    unsafe { SCM_DOUBLE_BUFFER_ENABLED.store(true, Ordering::SeqCst) };
    if SCM_FRAMEWORK_LIST.lock().is_empty() {
        // scm 框架链表为空
        return 0;
    }

    // 逐个检查已经注册了的ui框架，将其缓冲区更改为双缓冲
    for ptr in SCM_FRAMEWORK_LIST.lock().iter_mut() {
        if unsafe { buffer_equal(ptr.metadata().unwrap().buf.to_c(), video_frame_buffer_info) } {
            let message: *const u8 = "\ninit double buffer\n\0".as_ptr();
            c_uart_send_str(UartPort::COM1.to_u16(), message);
            let buf = ScmBufferInfo::new(ScmBfFlag::SCM_BF_DB | ScmBfFlag::SCM_BF_PIXEL);
            if buf.is_err() {
                return -1;
            }
            c_uart_send_str(
                UartPort::COM1.to_u16(),
                "\nto change double buffer\n\0".as_ptr(),
            );

            if ptr.change(buf.clone().unwrap()).unwrap() != 0 {
                let _ = destroy_buffer(&mut buf.unwrap().to_c());
            }
        }
    }

    // 设置定时刷新的对象
    unsafe {
        video_set_refresh_target(
            &mut CURRENT_FRAMEWORK_METADATA.lock().buf.to_c() as *mut scm_buffer_info_t
        )
    };
    // 通知显示驱动，启动双缓冲
    unsafe { video_reinitialize(true) };
    return 0;
}
/**
 * @brief 当内存管理单元被初始化之后，重新处理帧缓冲区问题
 *
 */
#[no_mangle]
pub extern "C" fn scm_reinit() -> i32 {
    unsafe { video_reinitialize(false) };

    // 遍历当前所有使用帧缓冲区的框架，更新地址
    // 逐个检查已经注册了的ui框架，将其缓冲区更改为双缓冲
    for ptr in SCM_FRAMEWORK_LIST.lock().iter_mut() {
        if unsafe { buffer_equal(ptr.metadata().unwrap().buf.to_c(), video_frame_buffer_info) } {
            let _ = ptr
                .change(unsafe { ScmBufferInfo::copy_from_c(&video_frame_buffer_info).unwrap() });
        }
    }

    return 0;
}
