#[derive(Debug, Clone, Copy, PartialEq, Eq, FromPrimitive, ToPrimitive)]
pub enum Type {
    Stream = 1,
    Datagram = 2,
    Raw = 3,
    RDM = 4,
    SeqPacket = 5,
    DCCP = 6,
    Packet = 10,
}

use crate::net::posix::PosixArgsSocketType;
impl TryFrom<PosixArgsSocketType> for Type {
    type Error = system_error::SystemError;
    fn try_from(x: PosixArgsSocketType) -> Result<Self, Self::Error> {
        use num_traits::FromPrimitive;
        return <Self as FromPrimitive>::from_u32(x.types().bits())
            .ok_or(system_error::SystemError::EINVAL);
    }
}
