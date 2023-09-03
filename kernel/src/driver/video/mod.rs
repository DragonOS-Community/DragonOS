use core::{
    cell::RefCell,
    ffi::{c_uint, c_void},
    intrinsics::unlikely,
    mem::MaybeUninit,
    ptr::copy_nonoverlapping,
    sync::atomic::{AtomicBool, Ordering},
};

use alloc::{boxed::Box, sync::Arc};

use crate::{
    arch::MMArch,
    include::bindings::bindings::{
        multiboot2_get_Framebuffer_info, multiboot2_iter, multiboot_tag_framebuffer_info_t,
        scm_buffer_info_t, FRAME_BUFFER_MAPPING_OFFSET, SCM_BF_FB,
        SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE,
    },
    kinfo,
    libs::{
        align::page_align_up,
        lib_ui::screen_manager::{ScmBuffer, ScmBufferFlag, ScmBufferInfo},
        rwlock::{RwLock, RwLockReadGuard},
        spinlock::SpinLock,
    },
    mm::{
        allocator::page_frame::PageFrameCount, kernel_mapper::KernelMapper,
        no_init::pseudo_map_phys, page::PageFlags, MemoryManagementArch, PhysAddr, VirtAddr,
    },
    syscall::SystemError,
    time::timer::{Timer, TimerFunction},
};

use super::uart::uart::{c_uart_send_str, UartPort::COM1};

static mut __MAMAGER: Option<VideoRefreshManager> = None;

pub fn video_refresh_manager() -> &'static VideoRefreshManager {
    return unsafe {
        &__MAMAGER
            .as_ref()
            .expect("Video refresh manager has not been initialized yet!")
    };
}

///管理显示刷新变量的结构体
pub struct VideoRefreshManager {
    device_buffer: RwLock<ScmBufferInfo>,
    fb_info: multiboot_tag_framebuffer_info_t,
    refresh_target: RwLock<Option<Arc<SpinLock<Box<[u32]>>>>>,
    running: AtomicBool,
}

const REFRESH_INTERVAL: u64 = 15;

impl VideoRefreshManager {
    /**
     * @brief 启动定时刷新
     * @return 启动成功: true, 失败: false
     */
    pub fn run_video_refresh(&self) -> bool {
        //设置Manager运行标志
        let res = self.set_run();

        //设置成功则开始任务，否则直接返回false
        if res {
            //第一次将expire_jiffies设置小一点，使得这次刷新尽快开始，后续的刷新将按照REFRESH_INTERVAL * 10的间隔进行
            let timer = Timer::new(VideoRefreshExecutor::new(), 1);
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
        let res = self
            .running
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed);
        if res.is_ok() {
            return true;
        } else {
            return false;
        }
    }

    /**
     * @brief VBE帧缓存区的地址重新映射
     * 将帧缓存区映射到地址SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE处
     */
    fn init_frame_buffer(&self) {
        kinfo!("Re-mapping VBE frame buffer...");
        let buf_vaddr = VirtAddr::new(
            SPECIAL_MEMOEY_MAPPING_VIRT_ADDR_BASE as usize + FRAME_BUFFER_MAPPING_OFFSET as usize,
        );

        let mut frame_buffer_info_graud = self.device_buffer.write();
        if let ScmBuffer::DeviceBuffer(vaddr) = &mut (frame_buffer_info_graud).buf {
            *vaddr = buf_vaddr;
        }

        // 地址映射
        let mut paddr = PhysAddr::new(self.fb_info.framebuffer_addr as usize);
        let count = PageFrameCount::new(
            page_align_up(frame_buffer_info_graud.buf_size()) / MMArch::PAGE_SIZE,
        );
        let page_flags: PageFlags<MMArch> = PageFlags::new().set_execute(true).set_write(true);

        let mut kernel_mapper = KernelMapper::lock();
        let mut kernel_mapper = kernel_mapper.as_mut();
        assert!(kernel_mapper.is_some());
        let mut vaddr = buf_vaddr;
        unsafe {
            for _ in 0..count.data() {
                let flusher = kernel_mapper
                    .as_mut()
                    .unwrap()
                    .map_phys(vaddr, paddr, page_flags)
                    .unwrap();

                flusher.flush();
                vaddr += MMArch::PAGE_SIZE;
                paddr += MMArch::PAGE_SIZE;
            }
        }

        kinfo!("VBE frame buffer successfully Re-mapped!");
    }

    /**
     * @brief 初始化显示模块，需先低级初始化才能高级初始化
     * @param level 初始化等级
     * false -> 低级初始化：不使用double buffer
     * true ->高级初始化：增加double buffer的支持
     * @return int
     */
    pub fn video_reinitialize(&self, level: bool) -> Result<(), SystemError> {
        if !level {
            self.init_frame_buffer();
        } else {
            // 开启屏幕计时刷新
            assert!(self.run_video_refresh());
        }
        return Ok(());
    }

    /**
     * @brief 设置帧缓冲区刷新目标
     *
     * @param buf
     * @return int
     */
    pub fn set_refresh_target(&self, buf_info: &ScmBufferInfo) -> Result<(), SystemError> {
        let mut refresh_target = self.refresh_target.write_irqsave();
        if let ScmBuffer::DoubleBuffer(double_buffer) = &buf_info.buf {
            *refresh_target = Some(double_buffer.clone());
            return Ok(());
        }
        return Err(SystemError::EINVAL);
    }

    pub fn refresh_target(&self) -> RwLockReadGuard<'_, Option<Arc<SpinLock<Box<[u32]>>>>> {
        let x = self.refresh_target.read();

        return x;
    }

    pub fn device_buffer(&self) -> RwLockReadGuard<'_, ScmBufferInfo> {
        return self.device_buffer.read();
    }

    /**
     * @brief 初始化显示驱动
     *
     * @return int
     */
    pub unsafe fn video_init() -> Result<(), SystemError> {
        static INIT: AtomicBool = AtomicBool::new(false);

        if INIT
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            panic!("Try to init video twice!");
        }

        let mut _reserved: u32 = 0;

        let mut fb_info: MaybeUninit<multiboot_tag_framebuffer_info_t> = MaybeUninit::uninit();
        //从multiboot2中读取帧缓冲区信息至fb_info
        multiboot2_iter(
            Some(multiboot2_get_Framebuffer_info),
            fb_info.as_mut_ptr() as usize as *mut c_void,
            &mut _reserved as *mut c_uint,
        );
        fb_info.assume_init();
        let fb_info: multiboot_tag_framebuffer_info_t = core::mem::transmute(fb_info);

        let width = fb_info.framebuffer_width;
        let height = fb_info.framebuffer_height;

        //初始化帧缓冲区信息结构体
        let (bit_depth, flags) = if fb_info.framebuffer_type == 2 {
            //当type=2时,width与height用字符数表示,故depth=8

            (8u32, ScmBufferFlag::SCM_BF_TEXT | ScmBufferFlag::SCM_BF_FB)
        } else {
            //否则为图像模式,depth应参照帧缓冲区信息里面的每个像素的位数
            (
                fb_info.framebuffer_bpp as u32,
                ScmBufferFlag::SCM_BF_PIXEL | ScmBufferFlag::SCM_BF_FB,
            )
        };

        let buf_vaddr = VirtAddr::new(0xffff800003000000);
        let device_buffer = ScmBufferInfo::new_device_buffer(
            width,
            height,
            width * height * ((bit_depth + 7) / 8),
            bit_depth,
            flags,
            buf_vaddr,
        )
        .unwrap();

        let init_text = "Video driver to map.\n";
        c_uart_send_str(COM1 as u16, init_text.as_ptr());

        //地址映射
        let paddr = PhysAddr::new(fb_info.framebuffer_addr as usize);
        let count = PageFrameCount::new(
            page_align_up(device_buffer.buf_size() as usize) / MMArch::PAGE_SIZE,
        );
        pseudo_map_phys(buf_vaddr, paddr, count);

        let result = Self {
            fb_info,
            device_buffer: RwLock::new(device_buffer),
            refresh_target: RwLock::new(None),
            running: AtomicBool::new(false),
        };

        __MAMAGER = Some(result);

        let init_text = "Video driver initialized.\n";
        c_uart_send_str(COM1 as u16, init_text.as_ptr());
        return Ok(());
    }
}

//刷新任务执行器
#[derive(Debug)]
struct VideoRefreshExecutor;

impl VideoRefreshExecutor {
    fn new() -> Box<VideoRefreshExecutor> {
        return Box::<VideoRefreshExecutor>::new(VideoRefreshExecutor);
    }
}

impl TimerFunction for VideoRefreshExecutor {
    /**
     * @brief 交给定时器执行的任务，此方法不应手动调用
     * @return Ok(())
     */
    fn run(&mut self) -> Result<(), SystemError> {
        // 获得Manager
        let manager = video_refresh_manager();

        let start_next_refresh = || {
            //判断是否还需要刷新，若需要则继续分配下一次计时任务，否则不分配
            if manager.running.load(Ordering::Acquire) {
                let timer = Timer::new(VideoRefreshExecutor::new(), 10 * REFRESH_INTERVAL);
                //将新一次定时任务加入队列
                timer.activate();
            }
        };

        let mut refresh_target: Option<RwLockReadGuard<'_, Option<Arc<SpinLock<Box<[u32]>>>>>> =
            None;
        for i in 0..10 {
            let g = manager.refresh_target.try_read();
            if g.is_none() {
                if i == 9 {
                    start_next_refresh();
                    return Ok(());
                }
                continue;
            }
            refresh_target = Some(g.unwrap());
            break;
        }

        let refresh_target = refresh_target.unwrap();

        if let ScmBuffer::DeviceBuffer(vaddr) = manager.device_buffer().buf {
            let p = vaddr.as_ptr() as *mut u8;
            unsafe {
                p.copy_from_nonoverlapping(
                    refresh_target.as_ref().unwrap().lock_irqsave().as_mut_ptr() as *mut u8,
                    manager.device_buffer().buf_size() as usize,
                )
            }
        }

        return Ok(());
    }
}

#[no_mangle]
pub unsafe extern "C" fn rs_video_init() -> i32 {
    return VideoRefreshManager::video_init()
        .map(|_| 0)
        .unwrap_or_else(|e| e.to_posix_errno());
}
