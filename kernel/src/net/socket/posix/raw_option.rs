/// SOL_RAW 层选项 (include/uapi/linux/icmp.h)
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromPrimitive, ToPrimitive)]
#[allow(non_camel_case_types)]
pub enum RawOption {
    ICMP_FILTER = 1,
}

impl TryFrom<u32> for RawOption {
    type Error = system_error::SystemError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        use num_traits::FromPrimitive;
        <Self as FromPrimitive>::from_u32(value).ok_or(system_error::SystemError::EINVAL)
    }
}
