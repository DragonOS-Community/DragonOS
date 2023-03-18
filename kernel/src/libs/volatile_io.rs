macro_rules! volatile_read {
    ($data: expr) => {
        unsafe { core::ptr::read_volatile(core::ptr::addr_of!($data)) }
    };
}

macro_rules! volatile_write {
    ($data: expr, $value: expr) => {
        unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!($data), $value) }
    };
}

/// @brief: 用于volatile设置某些bits
/// @param val: 设置这些位
/// @param flag: true表示设置这些位为1; false表示设置这些位为0;
macro_rules! volatile_set_bit {
    ($data: expr, $val: expr, $flag: expr) => {
        volatile_write!(
            $data,
            match $flag {
                true => core::ptr::read_volatile(core::ptr::addr_of!($data)) | $val,
                false => core::ptr::read_volatile(core::ptr::addr_of!($data)) & (!$val),
            }
        )
    };
}

/// @param data: volatile变量
/// @param bits: 置1的位才有效，表示写这些位
/// @param val: 要写的值
/// 比如: 写 x 的 2至8bit， 为 10, 可以这么写 volatile_write_bit(x, (1<<8)-(1<<2), 10<<2);    
macro_rules! volatile_write_bit {
    ($data: expr, $bits: expr, $val: expr) => {
        volatile_set_bit!($data, $bits, false);
        volatile_set_bit!($data, ($val) & ($bits), true);
    };
}
