#![allow(dead_code)]
use crate::include::bindings::bindings::pt_regs;

/// @brief 判断给定的栈帧是否来自用户态
/// 判断方法为：根据代码段选择子是否具有ring3的访问权限（低2bit均为1）
pub fn user_mode(regs: *const pt_regs)->bool{
    if (unsafe{(*regs).cs} & 0x3) != 0{
        return true;
    }else {
        return false;
    }
}