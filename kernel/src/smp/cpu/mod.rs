use core::sync::atomic::AtomicU32;

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

impl ProcessorId {
    pub const INVALID: ProcessorId = ProcessorId::new(u32::MAX);
}

static mut SMP_CPU_MANAGER: Option<SmpCpuManager> = None;

#[inline]
pub fn smp_cpu_manager() -> &'static SmpCpuManager {
    unsafe { SMP_CPU_MANAGER.as_ref().unwrap() }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CpuHpState {
    /// 启动阈值
    ThresholdBringUp = 0,

    /// 该CPU是离线的
    Offline,

    /// 该CPU是在线的
    Online,
}

/// Per-Cpu Cpu的热插拔状态
pub struct CpuHpCpuState {
    /// 当前状态
    state: CpuHpState,
    /// 目标状态
    target_state: CpuHpState,
    /// 指向热插拔的线程的PCB
    thread: Option<Arc<ProcessControlBlock>>,

    /// 当前是否为启动流程
    bringup: bool,
    /// 启动完成的信号
    comp_done_up: Completion,
}

impl CpuHpCpuState {
    const fn new() -> Self {
        Self {
            state: CpuHpState::Offline,
            target_state: CpuHpState::Offline,
            thread: None,
            bringup: false,
            comp_done_up: Completion::new(),
        }
    }

    #[allow(dead_code)]
    pub const fn thread(&self) -> &Option<Arc<ProcessControlBlock>> {
        &self.thread
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
    ///     访问，因此该函数只能在初始化阶段调用。
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

    #[allow(clippy::mut_from_ref)]
    fn cpuhp_state_mut(&self, cpu_id: ProcessorId) -> &mut CpuHpCpuState {
        unsafe { self.cpuhp_state.force_get_mut(cpu_id) }
    }

    /// 设置CPU的状态, 返回旧的状态
    pub unsafe fn set_cpuhp_state(
        &self,
        cpu_id: ProcessorId,
        target_state: CpuHpState,
    ) -> CpuHpState {
        let p = self.cpuhp_state.force_get_mut(cpu_id);
        let old_state = p.state;

        let bringup = target_state > p.state;
        p.target_state = target_state;
        p.bringup = bringup;

        return old_state;
    }

    pub fn set_online_cpu(&self, cpu_id: ProcessorId) {
        unsafe { self.set_cpuhp_state(cpu_id, CpuHpState::Online) };
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

        let cpu_state = self.cpuhp_state(cpu_id).state;
        debug!(
            "cpu_up: cpu_id: {}, cpu_state: {:?}, target_state: {:?}",
            cpu_id.data(),
            cpu_state,
            target_state
        );
        // 如果CPU的状态已经达到或者超过目标状态，则直接返回
        if cpu_state >= target_state {
            return Ok(());
        }

        unsafe { self.set_cpuhp_state(cpu_id, target_state) };
        let cpu_state = self.cpuhp_state(cpu_id).state;
        if cpu_state > CpuHpState::ThresholdBringUp {
            self.cpuhp_kick_ap(cpu_id, target_state)?;
        }

        return Ok(());
    }

    fn cpuhp_kick_ap(
        &self,
        cpu_id: ProcessorId,
        target_state: CpuHpState,
    ) -> Result<(), SystemError> {
        let prev_state = unsafe { self.set_cpuhp_state(cpu_id, target_state) };
        let hpstate = self.cpuhp_state_mut(cpu_id);
        if let Err(e) = self.do_cpuhp_kick_ap(hpstate) {
            self.cpuhp_reset_state(hpstate, prev_state);
            self.do_cpuhp_kick_ap(hpstate).ok();

            return Err(e);
        }

        return Ok(());
    }

    fn do_cpuhp_kick_ap(&self, cpu_state: &mut CpuHpCpuState) -> Result<(), SystemError> {
        let pcb = cpu_state.thread.as_ref().ok_or(SystemError::EINVAL)?;
        let cpu_id = pcb.sched_info().on_cpu().ok_or(SystemError::EINVAL)?;

        // todo: 等待CPU启动完成

        ProcessManager::wakeup(cpu_state.thread.as_ref().unwrap())?;

        CurrentSMPArch::start_cpu(cpu_id, cpu_state)?;
        assert_eq!(ProcessManager::current_pcb().preempt_count(), 0);
        self.wait_for_ap_thread(cpu_state, cpu_state.bringup);

        return Ok(());
    }

    fn wait_for_ap_thread(&self, cpu_state: &mut CpuHpCpuState, bringup: bool) {
        if bringup {
            cpu_state
                .comp_done_up
                .wait_for_completion()
                .expect("failed to wait ap thread");
        } else {
            todo!("wait_for_ap_thread")
        }
    }

    /// 完成AP的启动
    pub fn complete_ap_thread(&self, bringup: bool) {
        let cpu_id = smp_get_processor_id();
        let cpu_state = self.cpuhp_state_mut(cpu_id);
        if bringup {
            cpu_state.comp_done_up.complete();
        } else {
            todo!("complete_ap_thread")
        }
    }

    fn cpuhp_reset_state(&self, st: &mut CpuHpCpuState, prev_state: CpuHpState) {
        let bringup = !st.bringup;
        st.target_state = prev_state;

        st.bringup = bringup;
    }
}

pub fn smp_cpu_manager_init(boot_cpu: ProcessorId) {
    unsafe {
        SMP_CPU_MANAGER = Some(SmpCpuManager::new());
    }

    unsafe { smp_cpu_manager().set_possible_cpu(boot_cpu, true) };
    unsafe { smp_cpu_manager().set_present_cpu(boot_cpu, true) };

    SmpCpuManager::arch_init(boot_cpu);
}
