use crate::{
    arch::driver::sbi::ecall::{ecall0, ecall1},
    mm::VirtAddr,
};
use core::arch::asm;

/// `sbi_set_timer` extension ID
pub const SET_TIMER_EID: usize = 0x00;
/// `sbi_console_putchar` extension ID
pub const CONSOLE_PUTCHAR_EID: usize = 0x01;
/// `sbi_console_getchar` extension ID
pub const CONSOLE_GETCHAR_EID: usize = 0x02;
/// `sbi_clear_ipi` extension ID
pub const CLEAR_IPI_EID: usize = 0x03;
/// `sbi_send_ipi` extension ID
pub const SEND_IPI_EID: usize = 0x04;
/// `sbi_remote_fence_i` extension ID
pub const REMOTE_FENCE_I_EID: usize = 0x05;
/// `sbi_remote_sfence_vma` extension ID
pub const REMOTE_SFENCE_VMA_EID: usize = 0x06;
/// `sbi_remote_sfence_vma_asid` extension ID
pub const REMOTE_SFENCE_VMA_ASID_EID: usize = 0x07;
/// `sbi_shutdown` extension ID
pub const SHUTDOWN_EID: usize = 0x08;

/// 计划在未来的某个时间触发中断。
///
/// ## 参数
///
/// - `stime`：要触发中断的绝对时间，以滴答为单位。如果`stime`小于当前时间，则不会触发中断。
///
/// ## 详情
///
/// 要清除计时器中断而不预约另一个计时器事件，可以将时间设置为无限远（`u64::MAX`）或
/// mask `sie` CSR的`STIE` 位。此函数将清除待处理计时器中断位。
///
/// 注意：`time` 是一个绝对时间，不是从调用时刻开始的偏移量。这意味着如果您想要设置一个未来`n`和tick之后
/// 触发的时钟，您需要首先读取 `time` CSR，然后将滴答数添加到该值。关于如何确定每个滴答的时间，
/// 这是平台依赖的，而时钟频率应在 CPU 节点的 `timebase-frequency` 属性中表达，如果可用的话。
#[inline]
pub unsafe fn set_timer(stime: u64) {
    #[cfg(target_arch = "riscv64")]
    unsafe {
        ecall1(stime as usize, SET_TIMER_EID, 0).ok();
    }

    #[cfg(target_arch = "riscv32")]
    unsafe {
        asm!(
            "ecall",
            inout ("a0") stime as usize => _,
            inout ("a1") (stime >> 32) as usize => _,
            in("a7") SET_TIMER_EID,
        );
    }
}

/// 将字符写入调试控制台。如果仍有待处理的控制台输出，此调用将阻塞。如果不存在控制台，则不会执行任何操作。
#[inline]
pub unsafe fn console_putchar(c: u8) {
    unsafe {
        ecall1(c.into(), CONSOLE_PUTCHAR_EID, 0).ok();
    }
}

/// 尝试从调试控制台获取一个字符。
/// 如果没有任何字符等待阅读，或者没有调试控制台设备，则此函数将返回[`None`]。
#[inline]
pub unsafe fn console_getchar() -> Option<u8> {
    let mut ret: i8;

    unsafe {
        asm!(
            "ecall",
            lateout("a0") ret,
            in("a7") CONSOLE_GETCHAR_EID,
        );
    }

    match ret {
        -1 => None,
        _ => Some(ret as u8),
    }
}

/// 清除current核心的待处理中断（IPIs）。
#[inline]
#[deprecated = "S模式可以直接清除`sip.SSIP` CSR位，因此无需调用此函数。"]
pub unsafe fn clear_ipi() {
    unsafe {
        asm!(
            "ecall",
            in("a7") CLEAR_IPI_EID,
            lateout("a0") _,
        );
    }
}

/// 向所有由`hart_mask`位掩码指定的核心发送中断（IPI）。接收到的中断表示为监视器软件中断。
///
/// ## 参数
/// - `hart_mask`: 一个长度为`n_harts / size_of::<usize>()`的二进制位向量，向上取整到下一个`usize`。
#[inline]
pub unsafe fn send_ipi(hart_mask: &[usize]) {
    unsafe {
        asm!(
            "ecall",
            inlateout("a0") hart_mask.as_ptr() => _,
            in("a7") SEND_IPI_EID,
        );
    }
}

/// 对指定的心脏（hart）执行 `FENCE.I` 指令
///
/// ## 参数
/// - `hart_mask`: 一个长度为 `n_harts / size_of::<usize>()` 的位矢量，
/// 向上取整到下一个 `usize」。
#[inline]
pub unsafe fn remote_fence_i(hart_mask: &[usize]) {
    unsafe {
        asm!(
            "ecall",
            inlateout("a0") hart_mask.as_ptr() => _,
            in("a7") REMOTE_FENCE_I_EID,
        );
    }
}

/// 在指定的hart上执行`SFENCE.VMA`指令
/// 为指定的虚拟内存范围（由`start`和`size`指定）执行。
///
/// ## 参数
/// - `hart_mask`: 一个长度为`n_harts / size_of::<usize>()`的二进制向量，
/// 向上取整到下一个`usize`。
/// - `start`: 要执行`SFENCE.VMA`的起始虚拟地址。
/// - `size`: 要对`start`执行的`SFENCE.VMA`的字节大小。例如，要失效一个
/// 包含2个4-KiB页面的区域，您会为`size`传递`8192`。
///
/// 如果`start`和`size`都为`0`，或者如果`size`为[`usize::MAX`]，则将执行完整的
/// `SFENCE.VMA`，而不仅仅是一个或多个页面大小的`SFENCE.VMA`。
#[inline]
pub unsafe fn remote_sfence_vma(hart_mask: &[usize], start: VirtAddr, size: usize) {
    unsafe {
        asm!(
            "ecall",
            inlateout("a0") hart_mask.as_ptr() => _,
            in("a1") start.data(),
            in("a2") size,
            in("a7") REMOTE_SFENCE_VMA_EID,
        );
    }
}

/// 在指定的hart上执行SFENCE.VMA指令
///
/// 仅针对指定的地址空间ID（ASID）执行虚拟内存范围指定的
/// start和size的hart_mask位掩码。
///
/// ## 参数
/// - `hart_mask`: 一个长度为`n_harts / size_of::<usize>()`的二进制向量，
/// 向上取整到下一个`usize`。
/// - `start`: 要执行`SFENCE.VMA`的起始虚拟地址。
/// - `size`: 要对`start`执行的`SFENCE.VMA`的字节大小。例如，要失效一个
/// 包含2个4-KiB页面的区域，您会为`size`传递`8192`。
/// - `asid`: 要执行`SFENCE.VMA`的地址空间ID。
///
/// 如果start和size都为0，或者如果size为[usize::MAX]，则将执行全
/// 部SFENCE.VMA，而不是多个页面大小的SFENCE.VMA`。
#[inline]
pub unsafe fn remote_sfence_vma_asid(hart_mask: &[usize], start: usize, size: usize, asid: usize) {
    unsafe {
        asm!(
            "ecall",
            inlateout("a0") hart_mask.as_ptr() => _,
            in("a1") start,
            in("a2") size,
            in("a3") asid,
            in("a7") REMOTE_SFENCE_VMA_ASID_EID,
        );
    }
}

/// 将所有核心置于关闭状态，此时处理器的执行模式比当前监督模式具有更高的特权。此调用不会返回。
#[inline]
pub unsafe fn shutdown() -> ! {
    unsafe {
        asm!(
            "ecall",
            in("a7") SHUTDOWN_EID,
            options(noreturn)
        );
    }
}
