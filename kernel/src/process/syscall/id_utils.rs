//! ID 操作通用辅助函数
//!
//! 提供用于 setuid/setgid 相关系统调用的通用辅助函数，消除代码冗余

use crate::process::cred::{CAPFlags, Cred};
use system_error::SystemError;

/// 检查值是否为"不改变"标记（-1）
///
/// 在 setuid/setgid 相关系统调用中，-1（表示为 usize::MAX 或 u32::MAX as usize）
/// 表示不修改对应的字段
#[inline]
pub fn is_no_change(v: usize) -> bool {
    v == usize::MAX || v == u32::MAX as usize
}

/// 验证 ID 值的有效性
///
/// # 参数
/// - `v`: 要验证的 ID 值
///
/// # 返回
/// - `Ok(())`: 值有效
/// - `Err(SystemError::EINVAL)`: 值无效（超出 u32 范围且不是不改变标记）
#[inline]
pub fn validate_id(v: usize) -> Result<(), SystemError> {
    if is_no_change(v) {
        return Ok(());
    }
    if v > u32::MAX as usize {
        return Err(SystemError::EINVAL);
    }
    Ok(())
}

/// setreuid/setregid 的通用实现逻辑
///
/// # 参数
/// - `old_real`: 旧的 real ID (ruid/rgid)
/// - `old_eff`: 旧的 effective ID (euid/egid)
/// - `old_saved`: 旧的 saved ID (suid/sgid)
/// - `new_real`: 新的 real ID
/// - `new_eff`: 新的 effective ID
/// - `is_privileged`: 是否有特权
///
/// # 返回
/// - `Ok(())`: 允许操作
/// - `Err(SystemError::EPERM)`: 权限不足
pub fn check_setre_permissions(
    old_real: usize,
    old_eff: usize,
    old_saved: usize,
    new_real: usize,
    new_eff: usize,
    is_privileged: bool,
) -> Result<(), SystemError> {
    if new_real == old_real && new_eff == old_eff {
        return Ok(());
    }

    if !is_privileged {
        let allowed = |id: usize| -> bool { id == old_real || id == old_eff || id == old_saved };
        if (!is_no_change(new_real) && !allowed(new_real))
            || (!is_no_change(new_eff) && !allowed(new_eff))
        {
            return Err(SystemError::EPERM);
        }
    }

    Ok(())
}

/// setresuid/setresgid 的通用实现逻辑
///
/// # 参数
/// - `old_real`: 旧的 real ID
/// - `old_eff`: 旧的 effective ID
/// - `old_saved`: 旧的 saved ID
/// - `new_real`: 新的 real ID
/// - `new_eff`: 新的 effective ID
/// - `new_saved`: 新的 saved ID
/// - `is_privileged`: 是否有特权
///
/// # 返回
/// - `Ok(())`: 允许操作
/// - `Err(SystemError::EPERM)`: 权限不足
pub fn check_setres_permissions(
    old_real: usize,
    old_eff: usize,
    old_saved: usize,
    new_real: usize,
    new_eff: usize,
    new_saved: usize,
    is_privileged: bool,
) -> Result<(), SystemError> {
    if new_real == old_real && new_eff == old_eff && new_saved == old_saved {
        return Ok(());
    }

    if !is_privileged {
        let allowed = |id: usize| -> bool { id == old_real || id == old_eff || id == old_saved };
        if (!is_no_change(new_real) && !allowed(new_real))
            || (!is_no_change(new_eff) && !allowed(new_eff))
            || (!is_no_change(new_saved) && !allowed(new_saved))
        {
            return Err(SystemError::EPERM);
        }
    }

    Ok(())
}

/// 解析 ID 值：如果是不改变标记则返回旧值，否则返回新值
#[inline]
pub fn resolve_id(value: usize, old: usize) -> usize {
    if is_no_change(value) {
        old
    } else {
        value
    }
}

/// 验证 setuid/setgid 的 ID 值有效性
///
/// 与 validate_id 不同，setuid/setgid 不接受 -1（不改变标记），
/// 因此需要单独验证。
///
/// # 参数
/// - `v`: 要验证的 ID 值
///
/// # 返回
/// - `Ok(())`: 值有效
/// - `Err(SystemError::EINVAL)`: 值无效（为-1或超出u32范围）
#[inline]
pub fn validate_setuid_id(v: usize) -> Result<(), SystemError> {
    // setuid/setgid 不接受 -1
    if v == usize::MAX || v == u32::MAX as usize {
        return Err(SystemError::EINVAL);
    }
    if v > u32::MAX as usize {
        return Err(SystemError::EINVAL);
    }
    Ok(())
}

/// 处理 UID 变化后的 capability 更新
///
/// 根据 Linux capabilities(7) 手册和 Linux 内核实现（cap_emulate_setxuid），
/// 当 UID 发生变化时，需要相应地更新 capability 集合：
///
/// 1. 如果 {ruid, euid, suid} 都从至少有一个 0 变为全部非 0，
///    清除 permitted, effective 和 ambient capabilities
/// 2. 如果 euid 从 0 变为非 0，清除 effective capabilities
/// 3. 如果 euid 从非 0 变为 0，设置 effective = permitted
///
/// # 参数
/// - `new_cred`: 新的凭证（将被修改）
/// - `old_ruid`: 旧的 real UID
/// - `old_euid`: 旧的 effective UID
/// - `old_suid`: 旧的 saved UID
/// - `new_ruid`: 新的 real UID
/// - `new_euid`: 新的 effective UID
/// - `new_suid`: 新的 saved UID
pub fn handle_uid_capabilities(
    new_cred: &mut Cred,
    old_ruid: usize,
    old_euid: usize,
    old_suid: usize,
    new_ruid: usize,
    new_euid: usize,
    new_suid: usize,
) {
    let old_has_root = old_ruid == 0 || old_euid == 0 || old_suid == 0;
    let new_has_root = new_ruid == 0 || new_euid == 0 || new_suid == 0;

    // 规则 1: 所有 UID 都从 root 变为非 root，清除 permitted, effective, ambient
    if old_has_root && !new_has_root {
        new_cred.cap_permitted = CAPFlags::CAP_EMPTY_SET;
        new_cred.cap_effective = CAPFlags::CAP_EMPTY_SET;
        new_cred.cap_ambient = CAPFlags::CAP_EMPTY_SET;
    }

    // 规则 2: euid 从 root 变为非 root，清除 effective
    if old_euid == 0 && new_euid != 0 {
        new_cred.cap_effective = CAPFlags::CAP_EMPTY_SET;
    }

    // 规则 3: euid 从非 root 变为 root，设置 effective = permitted
    if old_euid != 0 && new_euid == 0 {
        new_cred.cap_effective = new_cred.cap_permitted;
    }
}

/// 处理 GID 变化后的 capability 更新
///
/// 注意：GID 变化通常不会直接影响 capability，但为了保持代码一致性
/// 和未来可能的扩展，我们提供这个函数。
/// 目前 GID 变化不改变 capability，但遵循与 UID 类似的逻辑结构。
///
/// # 参数
/// - `new_cred`: 新的凭证（将被修改）
/// - `old_rgid`: 旧的 real GID
/// - `old_egid`: 旧的 effective GID
/// - `old_sgid`: 旧的 saved GID
/// - `new_rgid`: 新的 real GID
/// - `new_egid`: 新的 effective GID
/// - `new_sgid`: 新的 saved GID
#[allow(dead_code)]
pub fn handle_gid_capabilities(
    _new_cred: &mut Cred,
    _old_rgid: usize,
    _old_egid: usize,
    _old_sgid: usize,
    _new_rgid: usize,
    _new_egid: usize,
    _new_sgid: usize,
) {
    // GID 变化不直接影响 capability
    // 但保留此函数以便未来扩展
}
