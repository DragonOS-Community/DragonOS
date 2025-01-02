use core::hint::spin_loop;

use system_error::SystemError;

use crate::{arch::cpu::cpu_reset, libs::mutex::Mutex, syscall::user_access::check_and_clone_cstr};

static SYSTEM_TRANSITION_MUTEX: Mutex<()> = Mutex::new(());

const LINUX_REBOOT_MAGIC1: u32 = 0xfee1dead;
const LINUX_REBOOT_MAGIC2: u32 = 672274793;
const LINUX_REBOOT_MAGIC2A: u32 = 85072278;
const LINUX_REBOOT_MAGIC2B: u32 = 369367448;
const LINUX_REBOOT_MAGIC2C: u32 = 537993216;

#[derive(Debug)]
pub enum RebootCommand {
    /// 重启系统，使用默认命令和模式
    Restart,
    /// 停止操作系统，并将系统控制权交给ROM监视器（如果有）
    Halt,
    /// Ctrl-Alt-Del序列导致执行RESTART命令
    CadOn,
    /// Ctrl-Alt-Del序列向init任务发送SIGINT信号
    CadOff,
    /// 停止操作系统，如果可能的话从系统中移除所有电源
    PowerOff,
    /// 使用给定的命令字符串重启系统
    Restart2,
    /// 使用软件挂起（如果编译在内）挂起系统
    SoftwareSuspend,
    /// 使用预先加载的Linux内核重启系统
    Kexec,
}

impl TryFrom<u32> for RebootCommand {
    type Error = SystemError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0x01234567 => Ok(RebootCommand::Restart),
            0xCDEF0123 => Ok(RebootCommand::Halt),
            0x89ABCDEF => Ok(RebootCommand::CadOn),
            0x00000000 => Ok(RebootCommand::CadOff),
            0x4321FEDC => Ok(RebootCommand::PowerOff),
            0xA1B2C3D4 => Ok(RebootCommand::Restart2),
            0xD000FCE2 => Ok(RebootCommand::SoftwareSuspend),
            0x45584543 => Ok(RebootCommand::Kexec),
            _ => Err(SystemError::EINVAL),
        }
    }
}

impl From<RebootCommand> for u32 {
    fn from(val: RebootCommand) -> Self {
        match val {
            RebootCommand::Restart => 0x01234567,
            RebootCommand::Halt => 0xCDEF0123,
            RebootCommand::CadOn => 0x89ABCDEF,
            RebootCommand::CadOff => 0x00000000,
            RebootCommand::PowerOff => 0x4321FEDC,
            RebootCommand::Restart2 => 0xA1B2C3D4,
            RebootCommand::SoftwareSuspend => 0xD000FCE2,
            RebootCommand::Kexec => 0x45584543,
        }
    }
}

/// 系统调用reboot的实现
///
/// 参考：https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/reboot.c#700
pub(super) fn do_sys_reboot(
    magic1: u32,
    magic2: u32,
    cmd: u32,
    arg: usize,
) -> Result<(), SystemError> {
    if magic1 != LINUX_REBOOT_MAGIC1
        || (magic2 != LINUX_REBOOT_MAGIC2
            && magic2 != LINUX_REBOOT_MAGIC2A
            && magic2 != LINUX_REBOOT_MAGIC2B
            && magic2 != LINUX_REBOOT_MAGIC2C)
    {
        return Err(SystemError::EINVAL);
    }
    let command = RebootCommand::try_from(cmd)?;
    let _guard = SYSTEM_TRANSITION_MUTEX.lock();
    log::debug!(
        "do_sys_reboot: magic1={}, magic2={}, cmd={:?}",
        magic1,
        magic2,
        command
    );
    match command {
        RebootCommand::Restart => kernel_restart(None),
        RebootCommand::Halt => kernel_halt(),
        RebootCommand::CadOn => {
            // todo: 支持Ctrl-Alt-Del序列
            return Ok(());
        }
        RebootCommand::CadOff => {
            // todo: 支持Ctrl-Alt-Del序列
            return Ok(());
        }
        RebootCommand::PowerOff => kernel_power_off(),
        RebootCommand::Restart2 => {
            let s = check_and_clone_cstr(arg as *const u8, Some(256))?;
            let cmd_str = s.to_str().map_err(|_| SystemError::EINVAL)?;
            kernel_restart(Some(cmd_str));
        }
        RebootCommand::SoftwareSuspend => {
            log::warn!("do_sys_reboot: SoftwareSuspend not implemented");
            return Err(SystemError::ENOSYS);
        }
        RebootCommand::Kexec => {
            log::warn!("do_sys_reboot: Kexec not implemented");
            return Err(SystemError::ENOSYS);
        }
    }
}

/// kernel_restart - 重启系统
///
/// ## 参数
/// - cmd: 指向包含重启命令的缓冲区的指针，或者 None
///
/// 关闭所有东西并执行一个干净的重启。
/// 在中断上下文中调用这是不安全的。
///
/// todo: 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/reboot.c#265
pub fn kernel_restart(cmd: Option<&str>) -> ! {
    if let Some(cmd) = cmd {
        log::warn!("Restarting system with command: '{}'", cmd);
    } else {
        log::warn!("Restarting system...");
    }
    unsafe { cpu_reset() }
}

/// todo: 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/reboot.c#678
pub fn kernel_power_off() -> ! {
    log::warn!("Power down");
    log::warn!("Currently, the system cannot be powered off, so we halt here.");
    loop {
        spin_loop();
    }
}

/// todo: 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/reboot.c#293
pub fn kernel_halt() -> ! {
    log::warn!("System halted.");
    loop {
        spin_loop();
    }
}
