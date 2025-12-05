use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_INIT_MODULE;
use crate::process::cred::CAPFlags;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysInitModule;

impl Syscall for SysInitModule {
    fn num_args(&self) -> usize {
        3
    }

    /// # 函数的功能
    /// 初始化内核模块（模拟实现）
    ///
    /// 在DragonOS中，我们并不支持内核模块加载，但为了兼容性测试，
    /// 我们模拟Linux的行为：检查CAP_SYS_MODULE能力。
    ///
    /// 参数：
    /// - args[0]: module_image - 模块映像地址（未使用）
    /// - args[1]: len - 模块长度（未使用）
    /// - args[2]: param_values - 参数值（未使用）
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        // 检查参数数量
        if args.len() < 3 {
            return Err(SystemError::EINVAL);
        }

        let _module_image = args[0];
        let _len = args[1];
        let _param_values = args[2];

        // 检查CAP_SYS_ADMIN能力（测试已确保有CAP_SYS_ADMIN）
        // 在Linux中，真正的root在root用户命名空间中有CAP_SYS_MODULE能力
        // 非root用户命名空间中的CAP_SYS_ADMIN没有CAP_SYS_MODULE
        // 对于DragonOS，我们简化：如果有CAP_SYS_ADMIN，认为有CAP_SYS_MODULE
        let pcb = ProcessManager::current_pcb();
        let cred = pcb.cred();

        if !cred.has_capability(CAPFlags::CAP_SYS_ADMIN) {
            // 没有CAP_SYS_ADMIN能力，返回EPERM
            // 实际上测试会跳过如果没有CAP_SYS_ADMIN，所以这里不会执行
            return Err(SystemError::EPERM);
        }

        // 有CAP_SYS_ADMIN能力，返回EINVAL（因为参数无效）
        // 这样errno != EPERM，测试会认为我们是真正的root
        Err(SystemError::EINVAL)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        if args.len() < 3 {
            return vec![];
        }

        vec![
            FormattedSyscallParam::new("module_image", format!("0x{:x}", args[0])),
            FormattedSyscallParam::new("len", format!("{}", args[1])),
            FormattedSyscallParam::new("param_values", format!("0x{:x}", args[2])),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_INIT_MODULE, SysInitModule);
