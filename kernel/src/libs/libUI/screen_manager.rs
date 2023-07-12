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

use super::textui::{
    renew_buf, textui_change_buf, TextuiPrivateInfo, CHAR_PER_LINE, TEXTUI_CHAR_HEIGHT,
    TEXTUI_CHAR_WIDTH, TRUE_LINE_NUM,
};

use lazy_static::lazy_static;
lazy_static! {
    pub static ref SCM_FRAMEWORK_LIST: SpinLock<LinkedList<Arc<dyn ScmUiFramework>>> =
        SpinLock::new(LinkedList::new());
}
lazy_static! {
    pub static ref CURRENT_FRAMEWORK_METADATA: RwLock<ScmUiFrameworkMetadata> =
        RwLock::new(ScmUiFrameworkMetadata::new(ScmFramworkType::Text));
}
pub static SCM_DOUBLE_BUFFER_ENABLED: AtomicBool = AtomicBool::new(false); // 允许双缓冲的标志位

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
#[allow(dead_code)]
pub enum ScmUiPrivateInfo {
    Textui(TextuiPrivateInfo),
    Gui,
    Unused,
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
    /**
     * @brief 创建新的帧缓冲区信息
     * @param buf 帧缓冲区
     * @param b_type 帧缓冲区类型
     * @return struct ScmBufferInfo 新的帧缓冲区结构体
     */
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
        self.vaddr.clone()
    }
    pub fn get_size_about_u8(&self) -> u32 {
        self.size.clone()
    }
    pub fn get_height_about_u32(&self) -> u32 {
        self.height.clone()
    }
    pub fn get_width_about_u32(&self) -> u32 {
        self.width.clone()
    }
    pub fn get_size_about_u32(&self) -> u32 {
        self.height.clone() * self.width.clone()
    }
}

impl From<&scm_buffer_info_t> for ScmBufferInfo {
    fn from(value: &scm_buffer_info_t) -> Self {
        Self {
            width: value.width,
            height: value.height,
            size: value.size,
            bit_depth: value.bit_depth,
            // buf,
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
    pub fn new() -> Self {
        static MAX_ID: AtomicU32 = AtomicU32::new(0);
        return ScmUiFrameworkId(MAX_ID.fetch_add(1, Ordering::SeqCst));
    }
}
#[derive(Debug, Clone)]
pub struct ScmUiFrameworkMetadata {
    pub id: ScmUiFrameworkId,
    pub name: String,
    pub f_type: ScmFramworkType,
    pub buf_info: ScmBufferInfo,
    // pub private_info: ScmUiPrivateInfo,
    pub is_enable: bool,
    pub window_max_id: u32,
}

impl ScmUiFrameworkMetadata {
    pub fn new(f_type: ScmFramworkType) -> Self {
        match f_type {
            ScmFramworkType::Text => {
                let result = ScmUiFrameworkMetadata {
                    // list: LinkedList::new(),
                    id: ScmUiFrameworkId::new(),
                    name: "".to_string(),
                    f_type: ScmFramworkType::Text,
                    buf_info: ScmBufferInfo::new(ScmBfFlag::SCM_BF_TEXT).unwrap(),
                    is_enable: false,
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

/**
 * @brief 初始化屏幕管理模块
 *
 */
#[no_mangle]
pub extern "C" fn scm_init() {
    SCM_DOUBLE_BUFFER_ENABLED.store(false, Ordering::SeqCst); // 禁用双缓冲

    //用于textui未初始化时
    no_texiui_init();

    c_uart_send_str(UartPort::COM1.to_u16(), "\nfinish_scm_init\n\0".as_ptr());
}
// 因为没有动态分配，texiui不能启动，只能暂时暴力往屏幕（video_frame_buffer_info）输出信息
fn no_texiui_init() {
    *TRUE_LINE_NUM.write() =
        unsafe { (video_frame_buffer_info.height / TEXTUI_CHAR_HEIGHT) as i32 };
    *CHAR_PER_LINE.write() = unsafe { (video_frame_buffer_info.width / TEXTUI_CHAR_WIDTH) as i32 };
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
    let mut current_framework = CURRENT_FRAMEWORK_METADATA.write();
    // 获取信息
    let metadata = framework.metadata()?;

    if SCM_DOUBLE_BUFFER_ENABLED.load(Ordering::SeqCst) == true {

        let buf:scm_buffer_info_t =framework.metadata()?.buf_info.into() ;
        let retval = unsafe { video_set_refresh_target(buf) };
        if retval == 0 {
            *current_framework = metadata;
            (*current_framework).is_enable = true;
        }
    } else {
        *current_framework = metadata;
        (*current_framework).is_enable = true;
    }

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
    framework.enable()?;
    // 如果当前还没有框架获得了屏幕的控制权，就让其拿去
    if !CURRENT_FRAMEWORK_METADATA.read().is_enable {
        return scm_framework_enable(framework);
    }

    return Ok(0);
}
/**
 * @brief 销毁双缓冲区
 *
 * @param buf
 * @return int
 */
// fn destroy_buffer(buf: &mut scm_buffer_info_t) -> Result<i32, SystemError> {
//     // 不能销毁帧缓冲区对象
//     if unsafe { scm_buffer_info_is_equal(*buf, video_frame_buffer_info) } {
//         return Err(SystemError::EINVAL);
//     }
//     if buf.vaddr == 0 {
//         return Err(SystemError::EINVAL);
//     }
//     if unsafe { verify_area(buf.vaddr as u64, buf.size.into()) } {
//         return Err(SystemError::EINVAL);
//     }
//     // 是否双缓冲区
//     if ScmBfFlag::from_bits_truncate(buf.flags as u8).contains(ScmBfFlag::SCM_BF_FB) {
//         return Err(SystemError::EINVAL);
//     }

//     // 释放内存页
//     let page_size = PAGE_2M_SIZE;
//     let page_align = PAGE_2M_ALIGN(unsafe { video_frame_buffer_info.size });
//     let page_count = page_align / page_size;
//     unsafe {
//         free_pages(
//             Phy_to_2M_Page(virt_2_phys(buf.vaddr as usize)),
//             page_count as i32,
//         )
//     };

//     return Ok(0);
// }
/**
 * @brief 允许双缓冲区
 *
 * @return int
 */
#[no_mangle]
pub extern "C" fn scm_enable_double_buffer() -> i32 {
    let r = ture_scm_enable_double_buffer()
        .map_err(|e| e.to_posix_errno())
        .unwrap();
    if r.is_negative() {
        c_uart_send_str(
            UartPort::COM1.to_u16(),
            "scm enable double buffer fail.\n\0".as_ptr(),
        );
    }

    return r;
}
fn ture_scm_enable_double_buffer() -> Result<i32, SystemError> {
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
    // 逐个检查已经注册了的ui框架，将其缓冲区更改为双缓冲(暂时还没想好怎么不用指针把各个框架的缓冲区更改为双缓冲区，先直接把textui框架的缓冲区更改为双缓冲)
    // for framework in scm_list.iter_mut() {
    //     if unsafe {
    //         scm_buffer_info_is_equal(
    //             framework.metadata()?.buf_info.into(),
    //             video_frame_buffer_info,
    //         )
    //     } {
    //         c_uart_send_str(UartPort::COM1.to_u16(), "\ninit double buffer\n\0".as_ptr());
    //         // 创建双缓冲区
    //         let buf_into = ScmBufferInfo::new(ScmBfFlag::SCM_BF_DB | ScmBfFlag::SCM_BF_PIXEL)?;
    //         if !framework.change(buf_into.clone()).is_err() {
    //             destroy_buffer(&mut buf_into.into())?;
    //         }
    //     }
    // }

    // 创建双缓冲区
    let buf_info = ScmBufferInfo::new(ScmBfFlag::SCM_BF_DB | ScmBfFlag::SCM_BF_PIXEL)?;

    (*CURRENT_FRAMEWORK_METADATA.write()).buf_info = buf_info.clone();
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
    let r = true_scm_reinit().map_err(|e| e.to_posix_errno()).unwrap();
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
