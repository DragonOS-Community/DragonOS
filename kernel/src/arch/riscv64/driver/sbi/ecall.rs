#![allow(dead_code)]
use super::SbiError;

/// 使用给定的扩展和函数 ID 进行零参数的 `ecall`。
///
/// # 安全性
/// 只有在给定的函数 ID 不接受任何参数时，调用此函数才是安全的，否则行为是未定义的，
/// 因为当传递给 SBI 实现时，额外的参数寄存器将具有未定义的内容。
#[inline]
pub unsafe fn ecall0(extension_id: usize, function_id: usize) -> Result<usize, SbiError> {
    let error: isize;
    let value: usize;

    core::arch::asm!(
        "ecall",
        in("a6") function_id,
        in("a7") extension_id,
        lateout("a0") error,
        lateout("a1") value,
    );

    match error {
        0 => Result::Ok(value),
        e => Result::Err(SbiError::new(e)),
    }
}

/// 使用给定的扩展和函数 ID 进行单参数的 `ecall`。
///
/// # 安全性
/// 只有在给定的函数 ID 接受一个参数时，调用此函数才是安全的，否则行为是未定义的，
/// 因为当传递给 SBI 实现时，额外的参数寄存器将具有未定义的内容。
#[inline]
pub unsafe fn ecall1(
    arg: usize,
    extension_id: usize,
    function_id: usize,
) -> Result<usize, SbiError> {
    let error: isize;
    let value: usize;

    core::arch::asm!(
        "ecall",
        inlateout("a0") arg => error,
        in("a6") function_id,
        in("a7") extension_id,
        lateout("a1") value,
    );

    match error {
        0 => Result::Ok(value),
        e => Result::Err(SbiError::new(e)),
    }
}

/// 一个带有给定扩展和函数ID的两参数`ecall`。
///
/// # 安全性
/// 只有在给定的函数ID接受两个参数时，才安全调用此函数。否则，行为将是未定义的，
/// 因为将额外的中断寄存器传递给SBI实现时，其内容将是未定义的。
#[inline]
pub unsafe fn ecall2(
    arg0: usize,
    arg1: usize,
    extension_id: usize,
    function_id: usize,
) -> Result<usize, SbiError> {
    let error: isize;
    let value: usize;

    core::arch::asm!(
        "ecall",
        inlateout("a0") arg0 => error,
        inlateout("a1") arg1 => value,
        in("a6") function_id,
        in("a7") extension_id,
    );

    match error {
        0 => Result::Ok(value),
        e => Result::Err(SbiError::new(e)),
    }
}

/// 使用给定的扩展和函数 ID 进行 3参数 的 `ecall`。
///
/// # 安全性
/// 只有在给定的函数 ID 接受一个参数时，调用此函数才是安全的，否则行为是未定义的，
/// 因为当传递给 SBI 实现时，额外的参数寄存器将具有未定义的内容。
#[inline]
pub unsafe fn ecall3(
    arg0: usize,
    arg1: usize,
    arg2: usize,
    extension_id: usize,
    function_id: usize,
) -> Result<usize, SbiError> {
    let error: isize;
    let value: usize;

    core::arch::asm!(
        "ecall",
        inlateout("a0") arg0 => error,
        inlateout("a1") arg1 => value,
        in("a2") arg2,
        in("a6") function_id,
        in("a7") extension_id,
    );

    match error {
        0 => Result::Ok(value),
        e => Result::Err(SbiError::new(e)),
    }
}

/// 使用给定的扩展和函数 ID 进行 4参数 的 `ecall`。
///
/// # 安全性
/// 只有在给定的函数 ID 接受一个参数时，调用此函数才是安全的，否则行为是未定义的，
/// 因为当传递给 SBI 实现时，额外的参数寄存器将具有未定义的内容。
#[inline]
pub unsafe fn ecall4(
    arg0: usize,
    arg1: usize,
    arg2: usize,
    arg3: usize,
    extension_id: usize,
    function_id: usize,
) -> Result<usize, SbiError> {
    let error: isize;
    let value: usize;

    core::arch::asm!(
        "ecall",
        inlateout("a0") arg0 => error,
        inlateout("a1") arg1 => value,
        in("a2") arg2,
        in("a3") arg3,
        in("a6") function_id,
        in("a7") extension_id,
    );

    match error {
        0 => Result::Ok(value),
        e => Result::Err(SbiError::new(e)),
    }
}

/// 使用给定的扩展和函数 ID 进行 5参数 的 `ecall`。
///
/// # 安全性
/// 只有在给定的函数 ID 接受一个参数时，调用此函数才是安全的，否则行为是未定义的，
/// 因为当传递给 SBI 实现时，额外的参数寄存器将具有未定义的内容。
#[inline]
pub unsafe fn ecall5(
    arg0: usize,
    arg1: usize,
    arg2: usize,
    arg3: usize,
    arg4: usize,
    extension_id: usize,
    function_id: usize,
) -> Result<usize, SbiError> {
    let error: isize;
    let value: usize;

    core::arch::asm!(
        "ecall",
        inlateout("a0") arg0 => error,
        inlateout("a1") arg1 => value,
        in("a2") arg2,
        in("a3") arg3,
        in("a4") arg4,
        in("a6") function_id,
        in("a7") extension_id,
    );

    match error {
        0 => Result::Ok(value),
        e => Result::Err(SbiError::new(e)),
    }
}

/// 使用给定的扩展和函数 ID 进行 6参数 的 `ecall`。
///
/// # 安全性
/// 只有在给定的函数 ID 接受一个参数时，调用此函数才是安全的，否则行为是未定义的，
/// 因为当传递给 SBI 实现时，额外的参数寄存器将具有未定义的内容。
#[inline]
#[allow(clippy::too_many_arguments)]
pub unsafe fn ecall6(
    arg0: usize,
    arg1: usize,
    arg2: usize,
    arg3: usize,
    arg4: usize,
    arg5: usize,
    extension_id: usize,
    function_id: usize,
) -> Result<usize, SbiError> {
    let error: isize;
    let value: usize;

    core::arch::asm!(
        "ecall",
        inlateout("a0") arg0 => error,
        inlateout("a1") arg1 => value,
        in("a2") arg2,
        in("a3") arg3,
        in("a4") arg4,
        in("a5") arg5,
        in("a6") function_id,
        in("a7") extension_id,
    );

    match error {
        0 => Result::Ok(value),
        e => Result::Err(SbiError::new(e)),
    }
}
