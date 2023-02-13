macro_rules! v_read {
    ($data: expr) => {
        unsafe { core::ptr::read_volatile(core::ptr::addr_of!($data)) }
    };
}

macro_rules! v_write {
    ($data: expr, $value: expr) => {
        unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!($data), $value) }
    };
}

macro_rules! v_set_bit {
    ($data: expr, $val: expr, $flag: expr) => {
        v_write!(
            $data,
            match $flag {
                true => v_read!($data) | $val,
                false => v_read!($data) & (!$val),
            }
        )
    };
}

macro_rules! v_write_bit {
    ($data: expr, $bits: expr, $val: expr) => {
        v_set_bit!($data, $bits, false);
        v_set_bit!($data, $val, true);
    };
}
