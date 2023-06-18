use crate::syscall::SystemError;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ApicTimerMode {
    /// 一次性定时模式。one-shot mode using a count-down value
    OneShot,
    /// 周期性定时模式。periodic mode using a count-down value
    Periodic,
    /// TSC截止时间模式。TSC-Deadline mode using absolute target value in IA32_TSC_DEADLINE MSR
    TSCDeadline,
}

impl TryFrom<u8> for ApicTimerMode {
    type Error = SystemError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0b00 => {
                return Ok(ApicTimerMode::OneShot);
            }
            0b01 => {
                return Ok(ApicTimerMode::Periodic);
            }
            0b10 => {
                return Ok(ApicTimerMode::TSCDeadline);
            }
            _ => {
                return Err(SystemError::EINVAL);
            }
        }
    }
}
