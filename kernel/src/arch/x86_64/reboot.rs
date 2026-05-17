use super::{
    driver::{
        apic::ioapic::IOAPIC,
        hpet::hpet_disable,
        rtc::{write_cmos, RTC_LOCK},
    },
    process::stop_this_cpu,
    CurrentIrqArch,
};
use crate::{
    arch::{driver::apic::CurrentApic, io::PortIOArch, CurrentPortIOArch, MMArch},
    driver::acpi::reboot::{acpi_poweroff_probe_status, acpi_reboot},
    exception::{
        ipi::{IpiKind, IpiTarget},
        InterruptArch,
    },
    libs::cpumask::CpuMask,
    misc::reboot::do_machine_power_off,
    mm::{MemoryManagementArch, PhysAddr},
    process::ProcessManager,
    sched::{request_task_migration, schedule, SchedMode},
    smp::{
        core::smp_get_processor_id,
        cpu::{smp_cpu_manager, ProcessorId},
    },
    time::{sleep::nanosleep, PosixTimeSpec},
};
use core::{
    arch::asm,
    hint::spin_loop,
    ptr,
    sync::atomic::{AtomicBool, Ordering},
};
use log::debug;
use x86::dtables::{lidt, DescriptorTablePointer};

#[derive(PartialEq, Clone)]
enum RebootType {
    /// 三重重启
    Triple,
    /// 键盘重启
    Kbd,
    /// BIOS重启
    Bios,
    /// EFI重启
    Acpi,
    /// EFI重启
    Efi,
    /// 强制CF9重启
    Cf9Force,
    /// 安全CF9重启
    Cf9Safe,
}

#[derive(PartialEq, Copy, Clone)]
enum RebootMode {
    RebootUndefined,
    // RebootCold,
    RebootWarm,
    // RebootHard,
    // RebootSoft,
    // RebootGpio,
}

static mut REBOOT_FORCE: bool = false;
static mut REBOOT_MODE: RebootMode = RebootMode::RebootUndefined;
static STOPPING_CPUS: AtomicBool = AtomicBool::new(false);
const STOP_OTHER_CPUS_TIMEOUT_LOOPS: usize = 10_000_000;

/// # 功能
///
/// 执行系统重启操作。该函数会尝试使用不同的方法来重启系统，直到成功为止。
///
///  参考: https://elixir.bootlin.com/linux/v6.6/source/arch/x86/kernel/reboot.c#L819
pub(crate) fn machine_restart(_cmd: Option<&str>) -> ! {
    debug!("machine restart");

    if !(unsafe { REBOOT_FORCE }) {
        machine_shutdown();
    }

    emergency_restart()
}

/// # 功能
///
/// 执行系统停止操作
pub(crate) fn machine_halt() -> ! {
    machine_shutdown();

    stop_this_cpu();
}

// 参考: https://elixir.bootlin.com/linux/v6.6/source/arch/x86/kernel/reboot.c#L782
pub(crate) fn machine_power_off() -> ! {
    if !(unsafe { REBOOT_FORCE }) {
        machine_shutdown();
    }

    if let Err(e) = do_machine_power_off() {
        log::warn!(
            "ACPI poweroff probe status: {}",
            acpi_poweroff_probe_status()
        );
        log::warn!(
            "All power-off handlers returned or failed: {:?}, halt instead",
            e
        );
    }

    stop_this_cpu();
}

/// # 功能
///
/// 执行系统关闭操作
///
/// 参考：https://code.dragonos.org.cn/xref/linux-6.1.9/arch/x86/kernel/reboot.c#675
fn machine_shutdown() {
    debug!("machine shutdown");
    // 在禁用本地APIC之前禁用IO APIC
    IOAPIC().lock_irqsave().disable_all();

    // 禁用本地中断
    unsafe {
        CurrentIrqArch::interrupt_disable();
    }

    stop_other_cpus();
    CurrentApic.lapic_shutdown();

    // 禁用HPET
    hpet_disable();
}

/// Migrate the current shutdown task to the reboot CPU.
///
/// Linux pins the shutdown task to reboot_cpu before syscore_shutdown, so the
/// later shutdown sequence does not run on an AP that is about to be stopped.
pub(crate) fn migrate_to_reboot_cpu() {
    let reboot_cpu = ProcessorId::new(0);
    let dest_cpu = if smp_cpu_manager().is_online_cpu(reboot_cpu) {
        reboot_cpu
    } else {
        smp_cpu_manager()
            .present_cpus()
            .iter_cpu()
            .find(|&cpu| smp_cpu_manager().is_online_cpu(cpu))
            .unwrap_or_else(smp_get_processor_id)
    };

    let current = ProcessManager::current_pcb();
    current
        .sched_info()
        .set_cpus_allowed(CpuMask::from_cpu(dest_cpu));

    if smp_get_processor_id() != dest_cpu {
        if let Err(e) = request_task_migration(&current, dest_cpu) {
            log::warn!(
                "migrate_to_reboot_cpu: failed to migrate to CPU {}: {e:?}",
                dest_cpu.data()
            );
            return;
        }
        schedule(SchedMode::SM_NONE);
    }
}

fn stop_other_cpus() {
    if STOPPING_CPUS.swap(true, Ordering::SeqCst) {
        return;
    }

    let this_cpu = smp_get_processor_id();
    let mut targets = CpuMask::new();

    for cpu in smp_cpu_manager().present_cpus().iter_cpu() {
        if cpu != this_cpu && smp_cpu_manager().is_online_cpu(cpu) {
            targets.set(cpu, true);
        }
    }

    if targets.is_empty() {
        return;
    }

    crate::arch::interrupt::ipi::send_ipi(IpiKind::StopCpu, IpiTarget::Other);

    for _ in 0..STOP_OTHER_CPUS_TIMEOUT_LOOPS {
        if targets
            .iter_cpu()
            .all(|cpu| !smp_cpu_manager().is_online_cpu(cpu))
        {
            return;
        }
        spin_loop();
    }

    for cpu in targets.iter_cpu() {
        if smp_cpu_manager().is_online_cpu(cpu) {
            log::warn!(
                "stop_other_cpus: CPU {} did not stop before timeout",
                cpu.data()
            );
        }
    }
}

/// # 功能
///
/// 执行紧急重启操作
fn emergency_restart() -> ! {
    debug!("emergency restart");
    // 重试次数
    let mut attempt = 0;
    // 默认重启类型时Acpi
    let mut reboot_type = RebootType::Acpi;
    // 记录最开始的重启类型
    let origin_reboot_type = reboot_type.clone();
    // 标记0xCF9端口是否安全使用，0xCF9是PCI相关的端口，用于系统复位
    let mut port_cf9_safe = false;

    let mode = if unsafe { REBOOT_MODE } == RebootMode::RebootWarm {
        0x1234
    } else {
        0
    };

    // 将重启类型写入0x472寄存器
    let address = unsafe { MMArch::phys_2_virt(PhysAddr::new(0x472)).unwrap() };
    unsafe { ptr::write_volatile(address.as_ptr::<u16>(), mode) };

    // 逐步尝试不同的重启方式
    loop {
        match reboot_type {
            RebootType::Acpi => {
                debug!("acpi reboot.");
                acpi_reboot();
                // ACPI重启失败，尝试键盘重启
                reboot_type = RebootType::Kbd;
            }
            RebootType::Kbd => {
                debug!("kbd reboot");
                // 重试10次键盘重启，每次等待50微妙
                for _ in 0..10 {
                    kb_wait();
                    let sleep_time = PosixTimeSpec {
                        tv_sec: 0,
                        tv_nsec: 50_000, // 50_000ns
                    };
                    let _ = nanosleep(sleep_time);
                    // 发送0xfe到键盘控制器以触发重启
                    unsafe { CurrentPortIOArch::out8(0x64, 0xfe) };
                    let sleep_time = PosixTimeSpec {
                        tv_sec: 0,
                        tv_nsec: 50_000, // 50_000ns
                    };
                    let _ = nanosleep(sleep_time);
                }

                // 如果这是第一次尝试键盘重启，且原始重启类型是ACPI，则再次尝试ACPI重启
                if attempt == 0 && origin_reboot_type == RebootType::Acpi {
                    attempt = 1;
                    reboot_type = RebootType::Acpi;
                } else {
                    // 否则转到EFI重启
                    reboot_type = RebootType::Efi;
                }
            }
            RebootType::Efi => {
                // TODO: 由于x86架构并没有进行efi_init()的操作，没有初始化efi的runtime服务，所以没法使用efi重启
                // efi_reboot();

                // 如果efi重启失败，转到BIOS重启
                reboot_type = RebootType::Bios;
            }
            RebootType::Bios => {
                debug!("bios reboot");
                // TODO: 由于x86架构并没有实现实模式跳板（即短暂将CPU模式切换回实模式）这个功能，故无法实现bios重启
                bios_reboot();

                // 如果到了这里，系统可能已经挂掉了，但我们依然会继续执行，转到CF9重启
                reboot_type = RebootType::Cf9Force;
            }
            RebootType::Cf9Force => {
                debug!("cf9 force reboot");
                port_cf9_safe = true;
                reboot_type = RebootType::Cf9Safe;
            }
            RebootType::Cf9Safe => {
                debug!("cf9 safe reboot");
                if port_cf9_safe {
                    // 根据重启模式选择不同重启代码
                    let reboot_code = if unsafe { REBOOT_MODE } == RebootMode::RebootWarm {
                        0x06
                    } else {
                        0x0E
                    };
                    let cf9 = unsafe { x86::io::inb(0xcf9) } & !reboot_code;
                    // 请求硬重启
                    unsafe {
                        CurrentPortIOArch::out8(0xcf9, cf9 | 2);
                    }
                    let sleep_time = PosixTimeSpec {
                        tv_sec: 0,
                        tv_nsec: 50_000, // 50000ns
                    };
                    let _ = nanosleep(sleep_time);
                    // 执行实际的重启
                    unsafe {
                        CurrentPortIOArch::out8(0xcf9, cf9 | reboot_code);
                    }
                    let sleep_time = PosixTimeSpec {
                        tv_sec: 0,
                        tv_nsec: 50_000, // 50000ns
                    };
                    let _ = nanosleep(sleep_time);
                }
                // CF9重启失败后，转到三重重启方式
                reboot_type = RebootType::Triple;
            }
            RebootType::Triple => {
                debug!("triple reboot");

                unsafe {
                    // 使中断系统失效
                    idt_invalidate();
                    // 执行int3指令，触发调试异常
                    asm!("int3");
                }

                // 到这里系统很大可能挂掉了
                reboot_type = RebootType::Kbd;
            }
        }
    }
}

/// # 等待键盘控制器准备好，以便可以向其发送数据
fn kb_wait() {
    for _ in 0..0x1_0000 {
        let status = unsafe { x86::io::inb(0x64) };
        if (status & 0x2) == 0 {
            break;
        }
    }
    // 延迟2微妙，避免过度占用CPU
    let sleep_time = PosixTimeSpec {
        tv_sec: 0,
        tv_nsec: 2_000, // 2000ns
    };
    let _ = nanosleep(sleep_time);
}

/// # BIOS重启
fn bios_reboot() {
    // 关闭本地中断
    unsafe {
        CurrentIrqArch::interrupt_disable();
    }

    // 将CMOS寄存器0x0F位置零
    let guard = RTC_LOCK.lock();
    write_cmos(0x0f, 0x00);
    drop(guard);

    // TODO: 切换到trampoline页表

    // TODO: 跳转到低地址的实模式代码，负责最终的重启
}

/// # 使IDT（中断描述符表）无效，即将IDT的基地址和大小都设为0，并加载它，使系统无法响应中断
unsafe fn idt_invalidate() {
    let idtp = DescriptorTablePointer::<usize> {
        base: ptr::null::<usize>(),
        limit: 0,
    };
    lidt(&idtp);
}
