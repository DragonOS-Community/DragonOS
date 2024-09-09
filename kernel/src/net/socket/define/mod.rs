mod option;
pub use option::Options;

mod option_level;
pub use option_level::OptionsLevel;

mod msg_flag;
pub use msg_flag::MessageFlag;

mod ipproto;
pub use ipproto::IPProtocol;

#[derive(Debug, Clone, Copy, PartialEq, Eq, FromPrimitive, ToPrimitive)]
pub enum Type {
    Datagram = 1,
    Stream = 2,
    Raw = 3,
    RDM = 4,
    SeqPacket = 5,
    DCCP = 6,
    Packet = 10,
}

use crate::net::syscall_util::SysArgSocketType;
impl TryFrom<SysArgSocketType> for Type {
    type Error = system_error::SystemError;
    fn try_from(x: SysArgSocketType) -> Result<Self, Self::Error> {
        use num_traits::FromPrimitive;
        return <Self as FromPrimitive>::from_u32(x.types().bits())
            .ok_or(system_error::SystemError::EINVAL);
    }
}
