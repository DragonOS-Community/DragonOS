use core::{
    sync::atomic::{AtomicBool, Ordering}, ffi::{c_void, c_uint}, intrinsics::unlikely, ptr::copy_nonoverlapping, mem::ManuallyDrop,
};

use alloc::{boxed::Box, sync::Arc};

use crate::{
    include::bindings::bindings::{scm_buffer_info_t, multiboot_tag_framebuffer_info_t, multiboot_tag_t, SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE, FRAME_BUFFER_MAPPING_OFFSET, multiboot2_iter, multiboot2_get_Framebuffer_info, SCM_BF_TEXT, SCM_BF_PIXEL, SCM_BF_FB}, kinfo, mm::{VirtAddr, PhysAddr, allocator::page_frame::PageFrameCount, page::PageFlags, kernel_mapper::KernelMapper, no_init::pseudo_map_phys, MemoryManagementArch}, libs::{spinlock::SpinLock, align::page_align_up, mutex::Mutex}, arch::{mm::barrier::mfence, MMArch}, time::timer::{TimerFunction, Timer}, syscall::SystemError,
};

use super::uart::uart::{c_uart_send_str,UartPort::COM1};

///管理显示刷新变量的结构体
struct VideoRefreshManager{
    frame_buffer_info: scm_buffer_info_t,
    fb_info: multiboot_tag_framebuffer_info_t,
    refresh_target: Option<Arc<scm_buffer_info_t>>,
    running: AtomicBool,
    refresh_lock: SpinLock<bool>,
}

lazy_static!{
    //显示刷新的全局管理者
    static ref MANAGER: SpinLock<VideoRefreshManager> = SpinLock::new(VideoRefreshManager::default());
}

const REFRESH_INTERVAL:u64 = 15;

impl VideoRefreshManager{
    /**
     * @brief 默认
     * @return VideoRefreshManager
     */
    fn default() -> Self{
        return VideoRefreshManager{
            frame_buffer_info: scm_buffer_info_t{
                width : 0,
                size : 0,
                height : 0,
                bit_depth : 0,
                vaddr : 0,
                flags : 0,
            },
            fb_info:multiboot_tag_framebuffer_info_t{
                tag_t :  multiboot_tag_t{
                    type_ : 0,
                    size : 0,
                },
                framebuffer_addr : 0,
                framebuffer_pitch : 0,
                framebuffer_width : 0,
                framebuffer_height : 0,
                framebuffer_bpp : 0,
                framebuffer_type : 0,
                reserved : 0,
            },
            refresh_target: None,
            //daemon_pcb: None,
            refresh_lock: SpinLock::new(true),
            running: AtomicBool::new(false)
        };
    }

    /**
     * @brief 启动定时刷新
     * @return 启动成功: true, 失败: false
     */
    pub fn run_video_refresh(&self) -> bool{
        //设置Manager运行标志
        let res = self.set_run();

        //设置成功则开始任务，否则直接返回false
        if res {
            //第一次将expire_jiffies设置小一点，使得这次刷新尽快开始，后续的刷新将按照REFRESH_INTERVAL * 10的间隔进行
            let timer = Timer::new(
                VideoRefreshExecutor::new(),
                1);
            //将新一次定时任务加入队列
            timer.activate();
        }
        return res;
    }

    /**
     * @brief 停止定时刷新
     */
    pub fn stop_video_refresh(&self) {
        self.running.store(false, Ordering::Release);
    }

    fn set_run(&self) -> bool {
        let res = 
                self.
                running.
                compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed);
        if res.is_ok() {
            return true;
        }else {
            return false
        }
    }

    /**
     * @brief VBE帧缓存区的地址重新映射
     * 将帧缓存区映射到地址SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE处
     */
    fn init_frame_buffer(&mut self) {
        kinfo!("Re-mapping VBE frame buffer...");
        unsafe { 
            self.frame_buffer_info.vaddr = SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE as u64 + FRAME_BUFFER_MAPPING_OFFSET as u64;

            //地址映射
            let mut vaddr = VirtAddr::new(self.frame_buffer_info.vaddr as usize);
            let mut paddr = PhysAddr::new(self.fb_info.framebuffer_addr as usize);
            let count = PageFrameCount::new(
                page_align_up(self.frame_buffer_info.size as usize) / MMArch::PAGE_SIZE
            );
            let page_flags: PageFlags<MMArch> = PageFlags::new().set_execute(true).set_write(true);

            // 不需要这三行代码，因为不用设置 PAGE_U_S
            // if flags & PAGE_U_S as usize != 0 {
            //     page_flags = page_flags.set_user(true);
            // }

            let mut kernel_mapper = KernelMapper::lock();
            let mut kernel_mapper = kernel_mapper.as_mut();
            assert!(kernel_mapper.is_some());
            for _ in 0..count.data(){
                let flusher = kernel_mapper
                    .as_mut()
                    .unwrap()
                    .map_phys(vaddr, paddr, page_flags)
                    .unwrap();

                flusher.flush();
                vaddr += MMArch::PAGE_SIZE;
                paddr += MMArch::PAGE_SIZE;
            }
        };
        kinfo!("VBE frame buffer successfully Re-mapped!");
    }

    /**
     * @brief 初始化显示模块，需先低级初始化才能高级初始化
     * @param level 初始化等级
     * false -> 低级初始化：不使用double buffer
     * true ->高级初始化：增加double buffer的支持
     * @return int
     */
    pub fn video_reinitialize(&mut self,level: bool) -> i32{
        if !level {
            self.init_frame_buffer();
        }else {
            //开启屏幕计时刷新
            assert!(self.run_video_refresh());
        }
        return 0;
    }

    /**
     * @brief 设置帧缓冲区刷新目标
     *
     * @param buf
     * @return int
     */
    pub unsafe fn video_set_refresh_target(&mut self,buf: *mut scm_buffer_info_t) -> ::core::ffi::c_int {
        let manually_dropped_ptr = ManuallyDrop::new(buf);
        self.refresh_target = Some(Arc::from_raw(*manually_dropped_ptr));
        return 0;
    }

    /**
     * @brief 初始化显示驱动
     *
     * @return int
     */
    pub unsafe fn video_init(&mut self) -> ::core::ffi::c_int {
        let mut _reserved: u32 = 0;
        //从multiboot2中读取帧缓冲区信息至fb_info
        multiboot2_iter(
            Some(multiboot2_get_Framebuffer_info),
            &mut self.fb_info as *mut multiboot_tag_framebuffer_info_t as usize as *mut c_void,
            &mut _reserved as *mut c_uint
        );
        mfence();

        //初始化帧缓冲区信息结构体
        if self.fb_info.framebuffer_type == 2 {
            //当type=2时,width与height用字符数表示,故depth=8
            self.frame_buffer_info.bit_depth = 8;
            self.frame_buffer_info.flags |= SCM_BF_TEXT as u64;
        }else {
            //否则为图像模式,depth应参照帧缓冲区信息里面的每个像素的位数
            self.frame_buffer_info.bit_depth = self.fb_info.framebuffer_bpp as u32;
            self.frame_buffer_info.flags |= SCM_BF_PIXEL as u64;
        }

        //初始化宽高
        self.frame_buffer_info.width = self.fb_info.framebuffer_width;
        self.frame_buffer_info.height = self.fb_info.framebuffer_height;

        self.frame_buffer_info.flags |= SCM_BF_FB as u64;

        //确保前面的值初始化完成再进行后面的操作
        mfence();
        //初始化size
        self.frame_buffer_info.size = 
        self.frame_buffer_info.width * self.frame_buffer_info.height * ((self.frame_buffer_info.bit_depth + 7)/8);

        // 先临时映射到该地址，稍后再重新映射   
        self.frame_buffer_info.vaddr = 0xffff800003000000;
        let init_text = "Video driver to map.\n";
        c_uart_send_str(COM1 as u16, init_text.as_ptr());

        //地址映射
        let vaddr = VirtAddr::new(self.frame_buffer_info.vaddr as usize);
        let paddr = PhysAddr::new(self.fb_info.framebuffer_addr as usize);
        let count = PageFrameCount::new(page_align_up(self.frame_buffer_info.size as usize) / MMArch::PAGE_SIZE);
        pseudo_map_phys(vaddr, paddr, count);

        mfence();
        let init_text = "Video driver initialized.\n";
        c_uart_send_str(COM1 as u16, init_text.as_ptr());
        return 0;
    }
}

//刷新任务执行器
#[derive(Debug)]
struct VideoRefreshExecutor;

impl VideoRefreshExecutor{
    fn new() -> Box::<VideoRefreshExecutor> {
        return Box::<VideoRefreshExecutor>::new(VideoRefreshExecutor);
    }
}

impl TimerFunction for VideoRefreshExecutor{
    /**
     * @brief 交给定时器执行的任务，此方法不应手动调用
     * @return Ok(())
     */
    fn run(&mut self) -> Result<(), SystemError> {
        //获得Manager
        let manager = MANAGER.lock();
        //进行刷新
        unsafe{
            if unlikely(manager.refresh_target.is_none()) {
                //若帧缓冲区信息结构体中虚拟地址已经被初始化，则进行数据拷贝
                if manager.frame_buffer_info.vaddr != 0 {
                    //上锁
                    manager.refresh_lock.lock();

                    //拷贝
                    copy_nonoverlapping(
                        manager.frame_buffer_info.vaddr as *const u64,
                        manager.refresh_target.clone().unwrap().vaddr as *mut u64,
                        manager.refresh_target.clone().unwrap().size as usize);
                }
            }

            //判断是否还需要刷新，若需要则继续分配下一次计时任务，否则不分配
            if manager.running.load(Ordering::Acquire) {
                let timer = Timer::new(
                    VideoRefreshExecutor::new(),
                    10 * REFRESH_INTERVAL);
                //将新一次定时任务加入队列
                timer.activate();
            }
        }
        return Ok(());
    }
}

/**
 * @brief 初始化显示模块，需先低级初始化才能高级初始化
 * @param level 初始化等级
 * false -> 低级初始化：不使用double buffer
 * true ->高级初始化：增加double buffer的支持
 * @return int
 */
pub unsafe extern "C" fn video_reinitialize(level: bool) -> i32 {
    let mut manager = MANAGER.lock();
    return manager.video_reinitialize(level);
}

/**
 * @brief 设置帧缓冲区刷新目标
 *
 * @param buf
 * @return int
 */
pub unsafe extern "C" fn video_set_refresh_target(buf: *mut scm_buffer_info_t) -> ::core::ffi::c_int {
    let mut manager = MANAGER.lock();
    return manager.video_set_refresh_target(buf);
}

/**
 * @brief 初始化显示驱动
 *
 * @return int
 */
pub unsafe extern "C" fn video_init() -> ::core::ffi::c_int {
    let mut manager = MANAGER.lock();
    return manager.video_init();
}