use core::arch::asm;

use crate::{
    exception::InterruptArch,
    futex::{
        constant::{FUTEX_OP_ADD, FUTEX_OP_ANDN, FUTEX_OP_OR, FUTEX_OP_SET, FUTEX_OP_XOR},
        futex::Futex,
    },
    mm::{verify_area, VirtAddr},
    syscall::SystemError,
};

use super::CurrentIrqArch;

impl Futex {
    /// ### 对futex进行操作
    ///
    /// 进入该方法会关闭中断保证修改的原子性，所以进入该方法前应确保中断锁已释放
    ///
    /// ### return uaddr原来的值
    #[allow(unused_assignments)]
    pub fn arch_futex_atomic_op_inuser(
        op: u32,
        oparg: u32,
        uaddr: VirtAddr,
    ) -> Result<u32, SystemError> {
        let guard = unsafe { CurrentIrqArch::save_and_disable_irq() };

        // 校验地址
        verify_area(uaddr, core::mem::size_of::<u32>())?;

        let mut oldval: usize = 0;

        // TODO: 下面的汇编抄得有问题
        match op {
            FUTEX_OP_SET => unsafe {
                asm!(
                    "lock xchgl [{0}], {1:e}",
                    inout(reg) uaddr.data() => oldval,
                    inout(reg) oparg => _,
                    lateout("eax") _,
                );
            },
            FUTEX_OP_ADD => unsafe {
                asm!(
                    "lock xaddl [{0}], {1:e}",
                    inout(reg) uaddr.data() => oldval,
                    inout(reg) oparg => _,
                    lateout("eax") _,
                );
            },
            FUTEX_OP_OR => unsafe {
                asm!(
                    "lock orl [{0}], {1:e}",
                    inout(reg) uaddr.data() => oldval,
                    inout(reg) oparg => _,
                    lateout("eax") _,
                );
            },
            FUTEX_OP_ANDN => unsafe {
                asm!(
                    "lock andl [{0}], {1:e}",
                    inout(reg) uaddr.data() => oldval,
                    inout(reg) oparg => _,
                    lateout("eax") _,
                );
            },
            FUTEX_OP_XOR => unsafe {
                asm!(
                    "lock xorl [{0}], {1:e}",
                    inout(reg) uaddr.data() => oldval,
                    inout(reg) oparg => _,
                    lateout("eax") _,
                );
            },
            _ => return Err(SystemError::ENOSYS),
        }

        drop(guard);

        Ok(oldval as u32)
    }
}
