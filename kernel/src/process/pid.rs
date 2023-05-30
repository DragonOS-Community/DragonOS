#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum PidType {
    /// pid类型是进程id
    PID = 1,
    TGID = 2,
    PGID = 3,
    SID = 4,
    MAX = 5,
}

/// 为PidType实现判断相等的trait
impl PartialEq for PidType {
    fn eq(&self, other: &PidType) -> bool {
        *self as u8 == *other as u8
    }
}
