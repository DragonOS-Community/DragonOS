use super::{append_nested_link_attr, parse_nested_link_attrs};
use alloc::vec::Vec;
use system_error::SystemError;

const IFLA_INFO_DATA: u16 = 2;
const IFLA_BR_STP_STATE: u16 = 5;
const STP_DISABLED: u32 = 0;

pub(super) fn validate_settings(data: &[u8]) -> Result<bool, SystemError> {
    let mut stp_state = None;
    for attr in parse_nested_link_attrs(data)? {
        if attr.kind != IFLA_BR_STP_STATE {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        // Linux's deprecated NLA_U32 policy accepts trailing bytes.
        let bytes = attr.payload.first_chunk().ok_or(SystemError::ERANGE)?;
        stp_state = Some(u32::from_ne_bytes(*bytes));
    }
    if stp_state.is_some_and(|value| value != STP_DISABLED) {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    Ok(stp_state.is_some())
}

pub(super) fn append_settings(info: &mut Vec<u8>) -> Result<(), SystemError> {
    let mut data = Vec::new();
    append_nested_link_attr(&mut data, IFLA_BR_STP_STATE, &STP_DISABLED.to_ne_bytes())?;
    append_nested_link_attr(info, IFLA_INFO_DATA, &data)
}
