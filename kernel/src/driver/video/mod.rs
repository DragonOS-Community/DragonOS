use core::sync::atomic::{AtomicBool, Ordering};

use crate::{
    arch::MMArch,
    init::boot_params,
    kinfo,
    libs::{
        align::page_align_up,
        lib_ui::screen_manager::{ScmBuffer, ScmBufferFlag, ScmBufferInfo},
        rwlock::{RwLock, RwLockReadGuard},
        spinlock::SpinLock,
    },
    mm::{
        allocator::page_frame::PageFrameCount, kernel_mapper::KernelMapper, page::PageFlags,
        MemoryManagementArch,
    },
    time::timer::{Timer, TimerFunction},
};
use alloc::{boxed::Box, sync::Arc};
use system_error::SystemError;

pub mod fbdev;

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
    refresh_target: RwLock<Option<Arc<SpinLock<Box<[u32]>>>>>,
    running: AtomicBool,
}

const REFRESH_INTERVAL: u64 = 30;

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
            //第一次将expire_jiffies设置小一点，使得这次刷新尽快开始，后续的刷新将按照REFRESH_INTERVAL间隔进行
            let timer = Timer::new(VideoRefreshExecutor::new(), 1);
            //将新一次定时任务加入队列
            timer.activate();
        }
        return res;
    }

    /// 停止定时刷新
    #[allow(dead_code)]
    pub fn stop_video_refresh(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    fn set_run(&self) -> bool {
        let res = self
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst);
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
        let buf_vaddr = boot_params()
            .read_irqsave()
            .screen_info
            .lfb_virt_base
            .unwrap();

        let mut frame_buffer_info_guard = self.device_buffer.write();
        if let ScmBuffer::DeviceBuffer(vaddr) = &mut (frame_buffer_info_guard).buf {
            *vaddr = buf_vaddr;
        }

        // 地址映射
        let mut paddr = boot_params().read().screen_info.lfb_base;
        let count = PageFrameCount::new(
            page_align_up(frame_buffer_info_guard.buf_size()) / MMArch::PAGE_SIZE,
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

    #[allow(dead_code)]
    pub fn refresh_target(&self) -> RwLockReadGuard<'_, Option<Arc<SpinLock<Box<[u32]>>>>> {
        let x = self.refresh_target.read();

        return x;
    }

    pub fn device_buffer(&self) -> RwLockReadGuard<'_, ScmBufferInfo> {
        return self.device_buffer.read();
    }

    /// 在riscv64平台下暂时不支持
    #[cfg(target_arch = "riscv64")]
    pub unsafe fn video_init() -> Result<(), SystemError> {
        return Err(SystemError::ENOSYS);
    }

    /// 此函数用于初始化显示驱动，为后续的图形输出做好准备。
    #[cfg(target_arch = "x86_64")]
    pub unsafe fn video_init() -> Result<(), SystemError> {
        use crate::{
            arch::driver::video::arch_video_early_init,
            driver::{
                serial::serial8250::send_to_default_serial8250_port,
                video::fbdev::base::BootTimeVideoType,
            },
        };

        arch_video_early_init()?;

        let boot_params_guard = boot_params().read();
        let screen_info = &boot_params_guard.screen_info;
        let buf_vaddr = screen_info.lfb_virt_base.unwrap();

        let buf_flag: ScmBufferFlag;
        let device_buffer: ScmBufferInfo;

        if screen_info.video_type == BootTimeVideoType::Mda {
            buf_flag = ScmBufferFlag::SCM_BF_TEXT | ScmBufferFlag::SCM_BF_FB;
            device_buffer = ScmBufferInfo::new_device_buffer(
                screen_info.origin_video_cols.into(),
                screen_info.origin_video_lines.into(),
                screen_info.lfb_size as u32,
                screen_info.lfb_depth.into(),
                buf_flag,
                buf_vaddr,
            )
            .unwrap();
        } else {
            // 图形模式
            buf_flag = ScmBufferFlag::SCM_BF_PIXEL | ScmBufferFlag::SCM_BF_FB;
            device_buffer = ScmBufferInfo::new_device_buffer(
                screen_info.lfb_width,
                screen_info.lfb_height,
                screen_info.lfb_size as u32,
                screen_info.lfb_depth.into(),
                buf_flag,
                buf_vaddr,
            )
            .unwrap();
        }

        let result = Self {
            device_buffer: RwLock::new(device_buffer),
            refresh_target: RwLock::new(None),
            running: AtomicBool::new(false),
        };

        __MAMAGER = Some(result);

        let init_text = "Video driver initialized.\n\0";
        send_to_default_serial8250_port(init_text.as_bytes());
        return Ok(());
    }
}

//刷新任务执行器
#[derive(Debug)]
struct VideoRefreshExecutor;

impl VideoRefreshExecutor {
    fn new() -> Box<VideoRefreshExecutor> {
        return Box::new(VideoRefreshExecutor);
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
            if manager.running.load(Ordering::SeqCst) {
                let timer = Timer::new(VideoRefreshExecutor::new(), REFRESH_INTERVAL);
                //将新一次定时任务加入队列
                timer.activate();
            }
        };

        let mut refresh_target: Option<RwLockReadGuard<'_, Option<Arc<SpinLock<Box<[u32]>>>>>> =
            None;
        const TRY_TIMES: i32 = 2;
        for i in 0..TRY_TIMES {
            let g = manager.refresh_target.try_read();
            if g.is_none() {
                if i == TRY_TIMES - 1 {
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
            let mut target_guard = None;
            for _ in 0..2 {
                if let Ok(guard) = refresh_target.as_ref().unwrap().try_lock_irqsave() {
                    target_guard = Some(guard);
                    break;
                }
            }
            if target_guard.is_none() {
                start_next_refresh();
                return Ok(());
            }
            let mut target_guard = target_guard.unwrap();
            unsafe {
                p.copy_from_nonoverlapping(
                    target_guard.as_mut_ptr() as *mut u8,
                    manager.device_buffer().buf_size() as usize,
                )
            }
        }

        start_next_refresh();

        return Ok(());
    }
}

#[no_mangle]
pub unsafe extern "C" fn rs_video_init() -> i32 {
    return VideoRefreshManager::video_init()
        .map(|_| 0)
        .unwrap_or_else(|e| e.to_posix_errno());
}
