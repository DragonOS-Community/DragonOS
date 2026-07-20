use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicI32, AtomicU32, Ordering},
};

use alloc::{sync::Arc, vec::Vec};
use log::{debug, error, info};
use system_error::SystemError;

use crate::{
    arch::CurrentSMPArch,
    libs::cpumask::CpuMask,
    mm::percpu::{PerCpu, PerCpuVar},
    process::{ProcessControlBlock, ProcessManager},
    sched::completion::Completion,
};

use super::{core::smp_get_processor_id, SMPArch};

int_like!(ProcessorId, AtomicProcessorId, u32, AtomicU32);

const AP_BRINGUP_RESULT_PENDING: i32 = 0;
const AP_BRINGUP_RESULT_SUCCESS: i32 = 1;

impl ProcessorId {
    pub const INVALID: ProcessorId = ProcessorId::new(u32::MAX);
}

static mut SMP_CPU_MANAGER: Option<SmpCpuManager> = None;

#[inline]
pub fn smp_cpu_manager_initialized() -> bool {
    unsafe { SMP_CPU_MANAGER.is_some() }
}

#[inline]
pub fn smp_cpu_manager() -> &'static SmpCpuManager {
    unsafe { SMP_CPU_MANAGER.as_ref().unwrap() }
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuHpState {
    /// 该CPU是离线的
    Offline = 0,

    /// BSP 已经发起启动，但 AP 尚未完成初始化。
    Starting,

    /// 该CPU是在线的
    Online,

    /// AP 已经运行并在初始化失败后进入终止 park，不能按 Offline 重试。
    FailedParked,
}

struct CpuHpControl {
    /// BSP 期望达到的目标状态。
    target_state: CpuHpState,
    /// 当前是否为启动流程。
    bringup: bool,
}

/// Per-Cpu Cpu的热插拔状态
pub struct CpuHpCpuState {
    /// 跨 CPU 发布的生命周期状态。
    state: AtomicU32,
    /// 指向热插拔的线程的PCB
    thread: Option<Arc<ProcessControlBlock>>,
    /// 仅由 BSP/hotplug coordinator 访问的普通控制字段。
    control: UnsafeCell<CpuHpControl>,
    /// 启动完成的信号
    comp_done_up: Completion,
    /// AP 初始化结果。AP 仅发布该原子消息，普通热插拔状态仍由 BSP 独占更新。
    bringup_result: AtomicI32,
}

// SAFETY: `state`, `bringup_result` and `comp_done_up` provide their own
// synchronization. `control` is private and is only accessed by the single
// BSP hotplug coordinator; APs never read or write it.
unsafe impl Sync for CpuHpCpuState {}

impl CpuHpCpuState {
    const fn new() -> Self {
        Self {
            state: AtomicU32::new(CpuHpState::Offline as u32),
            thread: None,
            control: UnsafeCell::new(CpuHpControl {
                target_state: CpuHpState::Offline,
                bringup: false,
            }),
            comp_done_up: Completion::new(),
            bringup_result: AtomicI32::new(AP_BRINGUP_RESULT_PENDING),
        }
    }

    #[allow(dead_code)]
    pub const fn thread(&self) -> &Option<Arc<ProcessControlBlock>> {
        &self.thread
    }

    #[inline]
    #[allow(dead_code)]
    pub fn state(&self) -> CpuHpState {
        match self.state.load(Ordering::Acquire) {
            value if value == CpuHpState::Offline as u32 => CpuHpState::Offline,
            value if value == CpuHpState::Starting as u32 => CpuHpState::Starting,
            value if value == CpuHpState::Online as u32 => CpuHpState::Online,
            value if value == CpuHpState::FailedParked as u32 => CpuHpState::FailedParked,
            _ => CpuHpState::FailedParked,
        }
    }

    #[inline]
    fn publish_state(&self, state: CpuHpState) {
        self.state.store(state as u32, Ordering::Release);
    }

    /// Update BSP-owned control state before publishing a bring-up request.
    ///
    /// # Safety
    ///
    /// Only the BSP/hotplug coordinator may call this method, and bring-up
    /// transactions for one CPU must remain serialized.
    #[inline]
    unsafe fn configure_bringup(&self, target_state: CpuHpState, bringup: bool) {
        let control = unsafe { &mut *self.control.get() };
        control.target_state = target_state;
        control.bringup = bringup;
    }

    /// Read BSP-owned control state while the coordinator owns the transaction.
    ///
    /// # Safety
    ///
    /// Only the BSP/hotplug coordinator may call this method.
    #[inline]
    unsafe fn bringup(&self) -> bool {
        unsafe { (*self.control.get()).bringup }
    }
}

pub struct SmpCpuManager {
    /// 可用的CPU
    possible_cpus: CpuMask,
    /// 出现的CPU
    present_cpus: CpuMask,
    /// 出现在系统中的CPU的数量
    present_cnt: AtomicU32,
    /// 可用的CPU的数量
    possible_cnt: AtomicU32,
    /// CPU的状态
    cpuhp_state: PerCpuVar<CpuHpCpuState>,
}

impl SmpCpuManager {
    fn new() -> Self {
        let possible_cpus = CpuMask::new();
        let present_cpus = CpuMask::new();
        let mut data = Vec::with_capacity(PerCpu::MAX_CPU_NUM as usize);
        for i in 0..PerCpu::MAX_CPU_NUM {
            let mut hpstate = CpuHpCpuState::new();
            hpstate.thread = Some(ProcessManager::idle_pcb()[i as usize].clone());
            data.push(hpstate);
        }
        let cpuhp_state = PerCpuVar::new(data).unwrap();

        Self {
            possible_cpus,
            present_cpus,
            cpuhp_state,
            present_cnt: AtomicU32::new(0),
            possible_cnt: AtomicU32::new(0),
        }
    }

    /// 设置可用的CPU
    ///
    /// # Safety
    ///
    /// - 该函数不会检查CPU的有效性，调用者需要保证CPU的有效性。
    /// - 由于possible_cpus是一个全局变量，且为了性能考虑，并不会加锁
    ///   访问，因此该函数只能在初始化阶段调用。
    pub unsafe fn set_possible_cpu(&self, cpu: ProcessorId, value: bool) {
        // 强制获取mut引用，因为该函数只能在初始化阶段调用
        let p = (self as *const Self as *mut Self).as_mut().unwrap();

        if let Some(prev) = p.possible_cpus.set(cpu, value) {
            if prev != value {
                if value {
                    p.possible_cnt
                        .fetch_add(1, core::sync::atomic::Ordering::SeqCst);
                } else {
                    p.possible_cnt
                        .fetch_sub(1, core::sync::atomic::Ordering::SeqCst);
                }
            }
        }
    }

    /// 获取可用的CPU
    pub fn possible_cpus(&self) -> &CpuMask {
        &self.possible_cpus
    }

    pub fn possible_cpus_count(&self) -> u32 {
        self.possible_cnt.load(core::sync::atomic::Ordering::SeqCst)
    }

    pub fn present_cpus_count(&self) -> u32 {
        self.present_cnt.load(core::sync::atomic::Ordering::SeqCst)
    }

    pub unsafe fn set_present_cpu(&self, cpu: ProcessorId, value: bool) {
        // 强制获取mut引用，因为该函数只能在初始化阶段调用
        let p = (self as *const Self as *mut Self).as_mut().unwrap();

        if let Some(prev) = p.present_cpus.set(cpu, value) {
            if prev != value {
                if value {
                    p.present_cnt
                        .fetch_add(1, core::sync::atomic::Ordering::SeqCst);
                } else {
                    p.present_cnt
                        .fetch_sub(1, core::sync::atomic::Ordering::SeqCst);
                }
            }
        }
    }

    /// 获取CPU的状态
    pub fn cpuhp_state(&self, cpu_id: ProcessorId) -> &CpuHpCpuState {
        unsafe { self.cpuhp_state.force_get(cpu_id) }
    }

    pub fn set_online_cpu(&self, cpu_id: ProcessorId, is_online: bool) {
        let target_state = if is_online {
            CpuHpState::Online
        } else {
            CpuHpState::Offline
        };
        self.cpuhp_state(cpu_id).publish_state(target_state);
    }

    #[inline]
    #[allow(dead_code)]
    pub fn is_online_cpu(&self, cpu_id: ProcessorId) -> bool {
        self.cpuhp_state(cpu_id).state.load(Ordering::Acquire) == CpuHpState::Online as u32
    }

    /// 获取出现在系统中的CPU
    #[allow(dead_code)]
    pub fn present_cpus(&self) -> &CpuMask {
        &self.present_cpus
    }

    /// 启动bsp以外的CPU
    pub(super) fn bringup_nonboot_cpus(&self) {
        for cpu_id in self.present_cpus().iter_cpu() {
            if cpu_id == smp_get_processor_id() {
                continue;
            }

            debug!("Bring up CPU {}", cpu_id.data());

            if let Err(e) = self.cpu_up(cpu_id, CpuHpState::Online) {
                error!("Failed to bring up CPU {}: {:?}", cpu_id.data(), e);
            }
        }

        info!("All non-boot CPUs have been brought up");
    }

    fn cpu_up(&self, cpu_id: ProcessorId, target_state: CpuHpState) -> Result<(), SystemError> {
        if !self.possible_cpus().get(cpu_id).unwrap_or(false) {
            return Err(SystemError::EINVAL);
        }

        let cpu_state = self.cpuhp_state(cpu_id).state();
        debug!(
            "cpu_up: cpu_id: {}, cpu_state: {:?}, target_state: {:?}",
            cpu_id.data(),
            cpu_state,
            target_state
        );
        match cpu_state {
            CpuHpState::Online if target_state == CpuHpState::Online => return Ok(()),
            CpuHpState::Offline => {}
            CpuHpState::Starting | CpuHpState::FailedParked | CpuHpState::Online => {
                return Err(SystemError::EBUSY)
            }
        }

        self.cpuhp_kick_ap(cpu_id, target_state)
    }

    fn cpuhp_kick_ap(
        &self,
        cpu_id: ProcessorId,
        target_state: CpuHpState,
    ) -> Result<(), SystemError> {
        let cpu_state = self.cpuhp_state(cpu_id);
        let prev_state = cpu_state.state();
        unsafe { cpu_state.configure_bringup(target_state, true) };
        cpu_state.publish_state(CpuHpState::Starting);

        if let Err(e) = self.do_cpuhp_kick_ap(cpu_id) {
            let ap_parked = cpu_state.bringup_result.load(Ordering::Acquire) < 0;
            cpu_state.publish_state(if ap_parked {
                CpuHpState::FailedParked
            } else {
                prev_state
            });
            unsafe { cpu_state.configure_bringup(prev_state, false) };
            return Err(e);
        }

        cpu_state.publish_state(target_state);
        unsafe { cpu_state.configure_bringup(target_state, false) };

        Ok(())
    }

    fn do_cpuhp_kick_ap(&self, cpu_id: ProcessorId) -> Result<(), SystemError> {
        let cpu_state = self.cpuhp_state(cpu_id);
        let pcb = cpu_state.thread.as_ref().ok_or(SystemError::EINVAL)?;
        let target_cpu_id = pcb.sched_info().on_cpu().ok_or(SystemError::EINVAL)?;
        if target_cpu_id != cpu_id {
            return Err(SystemError::EINVAL);
        }
        let bringup = unsafe { cpu_state.bringup() };
        cpu_state
            .bringup_result
            .store(AP_BRINGUP_RESULT_PENDING, Ordering::Release);

        // todo: 等待CPU启动完成

        ProcessManager::wakeup(pcb)?;

        CurrentSMPArch::start_cpu(cpu_id, cpu_state)?;
        assert_eq!(ProcessManager::current_pcb().preempt_count(), 0);
        self.wait_for_ap_thread(cpu_id, bringup)?;

        return Ok(());
    }

    fn wait_for_ap_thread(&self, cpu_id: ProcessorId, bringup: bool) -> Result<(), SystemError> {
        if bringup {
            let cpu_state = self.cpuhp_state(cpu_id);
            cpu_state.comp_done_up.wait_for_completion()?;
            match cpu_state.bringup_result.load(Ordering::Acquire) {
                AP_BRINGUP_RESULT_SUCCESS => {}
                error if error < 0 => {
                    return Err(match SystemError::from_posix_errno(error) {
                        Some(error) => error,
                        None => SystemError::EIO,
                    });
                }
                _ => return Err(SystemError::EIO),
            }
        } else {
            todo!("wait_for_ap_thread")
        }
        Ok(())
    }

    /// 完成AP的启动
    pub fn complete_ap_thread(&self, bringup: bool, result: Result<(), SystemError>) {
        let cpu_id = smp_get_processor_id();
        let cpu_state = self.cpuhp_state(cpu_id);
        if bringup {
            let encoded = match result {
                Ok(()) => AP_BRINGUP_RESULT_SUCCESS,
                Err(error) => error.to_posix_errno(),
            };
            cpu_state.bringup_result.store(encoded, Ordering::Release);
            cpu_state.comp_done_up.complete();
        } else {
            todo!("complete_ap_thread")
        }
    }
}

pub fn smp_cpu_manager_init(boot_cpu: ProcessorId) {
    unsafe {
        SMP_CPU_MANAGER = Some(SmpCpuManager::new());
    }

    unsafe { smp_cpu_manager().set_possible_cpu(boot_cpu, true) };
    unsafe { smp_cpu_manager().set_present_cpu(boot_cpu, true) };
    smp_cpu_manager().set_online_cpu(boot_cpu, true);

    SmpCpuManager::arch_init(boot_cpu);
}
