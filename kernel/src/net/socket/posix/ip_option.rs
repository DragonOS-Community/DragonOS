/// SOL_IP 层选项 (include/uapi/linux/in.h)
///
/// 按 Linux 选项号定义为枚举，便于统一复用与避免 magic number。
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromPrimitive, ToPrimitive)]
#[allow(non_camel_case_types)]
pub enum IpOption {
    TOS = 1,
    TTL = 2,
    HDRINCL = 3,
    PKTINFO = 8,
    RECVTTL = 12,
    RECVTOS = 13,
}

impl TryFrom<u32> for IpOption {
    type Error = system_error::SystemError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        use num_traits::FromPrimitive;
        <Self as FromPrimitive>::from_u32(value).ok_or(system_error::SystemError::EINVAL)
    }
}
