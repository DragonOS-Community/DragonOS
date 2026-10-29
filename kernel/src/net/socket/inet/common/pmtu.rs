//! Linux IP{,V6}_MTU_DISCOVER policies. Route learning and application error
//! reporting deliberately remain separate decisions at the protocol layer.
use system_error::SystemError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(i32)]
pub(crate) enum PmtuPolicy {
    Dont = 0,
    #[default]
    Want = 1,
    Do = 2,
    Probe = 3,
    Interface = 4,
    Omit = 5,
}

impl PmtuPolicy {
    pub(crate) fn from_i32(value: i32) -> Result<Self, SystemError> {
        match value {
            0 => Ok(Self::Dont),
            1 => Ok(Self::Want),
            2 => Ok(Self::Do),
            3 => Ok(Self::Probe),
            4 => Ok(Self::Interface),
            5 => Ok(Self::Omit),
            _ => Err(SystemError::EINVAL),
        }
    }

    pub(crate) fn as_i32(self) -> i32 {
        self as i32
    }
    pub(crate) fn accepts_updates(self) -> bool {
        !matches!(self, Self::Interface | Self::Omit)
    }
    pub(crate) fn uses_path_mtu(self) -> bool {
        matches!(self, Self::Dont | Self::Want | Self::Do)
    }
    pub(crate) fn allows_fragmentation(self) -> bool {
        matches!(self, Self::Dont | Self::Want | Self::Omit)
    }
    pub(crate) fn effective_mtu(self, interface: usize, path: usize) -> usize {
        if self.uses_path_mtu() {
            interface.min(path)
        } else {
            interface
        }
    }
    pub(crate) fn ipv4_df(self, packet_len: usize, mtu: usize, locked: bool) -> bool {
        matches!(self, Self::Do | Self::Probe)
            || (self == Self::Want && !locked && packet_len <= mtu)
    }
}
