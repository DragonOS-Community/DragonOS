//! Linux ctnetlink read/delete policies. Legacy tuple nesting does not require
//! NLA_F_NESTED; duplicate attributes use the last value.
use super::super::nft::NlaIter;
use crate::net::conntrack::{CtAddress, CtFilter, CtL4, CtTuple, CtTupleFilter};
use system_error::SystemError;
const ALL: u32 = (1 << 12) - 1;
const CTA_FILTER: usize = 25;
const CTA_STATUS_MASK: usize = 26;

fn attrs<'a, const N: usize>(
    bytes: &'a [u8],
    lengths: &[(u16, usize)],
    nested: &[u16],
) -> Result<[Option<&'a [u8]>; N], SystemError> {
    let mut result = [None; N];
    for attr in NlaIter::new(bytes) {
        let attr = attr?;
        // Linux validates every occurrence before replacing the last value.
        // A valid duplicate must never hide malformed input before a delete.
        if lengths
            .iter()
            .any(|&(kind, len)| attr.kind == kind && attr.value.len() < len)
            || (nested.contains(&attr.kind) && !attr.value.is_empty() && attr.value.len() < 4)
        {
            return Err(SystemError::ERANGE);
        }
        if let Some(slot) = result.get_mut(attr.kind as usize) {
            *slot = Some(attr.value);
        }
    }
    Ok(result)
}
fn required(value: Option<&[u8]>) -> Result<&[u8], SystemError> {
    value.ok_or(SystemError::EINVAL)
}
fn fixed<const N: usize>(value: &[u8]) -> Result<[u8; N], SystemError> {
    value
        .get(..N)
        .ok_or(SystemError::ERANGE)?
        .try_into()
        .map_err(|_| SystemError::ERANGE)
}
pub(super) struct CtAttrs<'a> {
    fields: [Option<&'a [u8]>; 28],
}
impl<'a> CtAttrs<'a> {
    pub(super) fn new(bytes: &'a [u8]) -> Result<Self, SystemError> {
        // Validate policy even for metadata ignored by exact lookup. Docker
        // replays the complete dump, not just its tuple.
        // NLA_NESTED permits an empty payload but not a truncated child
        // header, even when this operation ignores that nested metadata.
        let fields = attrs(
            bytes,
            &[
                (3, 4),
                (7, 4),
                (8, 4),
                (12, 4),
                (18, 2),
                (21, 4),
                (CTA_STATUS_MASK as u16, 4),
            ],
            &[1, 2, 4, 5, 6, 13, 14, 15, 16, CTA_FILTER as u16],
        )?;
        Ok(Self { fields })
    }
    pub(super) fn id(&self) -> Option<u32> {
        self.fields[12].map(|v| u32::from_be_bytes(v[..4].try_into().unwrap()))
    }
    pub(super) fn needs_filter(&self, family: u8) -> bool {
        family != 0
            || self.fields[3].is_some()
            || self.fields[8].is_some()
            || self.fields[CTA_FILTER].is_some()
    }
    fn zone(&self) -> Result<(), SystemError> {
        // CONFIG_NF_CONNTRACK_ZONES=n: never silently alias a nonzero zone.
        if self.fields[18].is_some() {
            Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
        } else {
            Ok(())
        }
    }
    pub(super) fn exact(&self, family: u8) -> Result<Option<CtTuple>, SystemError> {
        self.zone()?;
        let Some(bytes) = self.fields[1].or(self.fields[2]) else {
            return Ok(None);
        };
        let value = tuple_filter(bytes, family, ALL)?;
        let l4 = match value.protocol.ok_or(SystemError::EINVAL)? {
            6 => CtL4::Tcp {
                src_port: value.src_port.ok_or(SystemError::EINVAL)?,
                dst_port: value.dst_port.ok_or(SystemError::EINVAL)?,
            },
            17 => CtL4::Udp {
                src_port: value.src_port.ok_or(SystemError::EINVAL)?,
                dst_port: value.dst_port.ok_or(SystemError::EINVAL)?,
            },
            1 => CtL4::Icmp {
                identifier: value.icmp_id.ok_or(SystemError::EINVAL)?,
                kind: value.icmp_type.ok_or(SystemError::EINVAL)?,
                code: value.icmp_code.ok_or(SystemError::EINVAL)?,
            },
            58 => CtL4::Icmpv6 {
                identifier: value.icmp_id.ok_or(SystemError::EINVAL)?,
                kind: value.icmp_type.ok_or(SystemError::EINVAL)?,
                code: value.icmp_code.ok_or(SystemError::EINVAL)?,
            },
            protocol => CtL4::Generic { protocol },
        };
        Ok(Some(CtTuple {
            src: value.src.ok_or(SystemError::EINVAL)?,
            dst: value.dst.ok_or(SystemError::EINVAL)?,
            l4,
        }))
    }
    pub(super) fn filter(&self, family: u8, flush: bool) -> Result<CtFilter, SystemError> {
        if flush && self.fields[CTA_FILTER].is_some() {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        if self.fields[8].is_some() || self.fields[21].is_some() {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        let status = if let Some(value) = self.fields[3] {
            let value = u32::from_be_bytes(fixed(value)?);
            let mask = self.fields[CTA_STATUS_MASK]
                .map(|v| fixed(v).map(u32::from_be_bytes))
                .transpose()?
                .unwrap_or(value);
            if mask == 0 {
                return Err(SystemError::EINVAL);
            }
            Some((value, mask))
        } else {
            if self.fields[CTA_STATUS_MASK].is_some() {
                return Err(SystemError::EINVAL);
            }
            None
        };
        let mut result = CtFilter {
            family,
            status,
            ..Default::default()
        };
        if let Some(bytes) = self.fields[CTA_FILTER] {
            self.zone()?;
            let flags = attrs::<3>(bytes, &[(1, 4), (2, 4)], &[])?;
            for (direction, output) in [(1, &mut result.original), (2, &mut result.reply)] {
                let flags = flags[direction]
                    .map(|v| fixed(v).map(u32::from_ne_bytes))
                    .transpose()?
                    .unwrap_or(0);
                if flags & !ALL != 0 {
                    return Err(SystemError::EINVAL);
                }
                if flags != 0 {
                    *output = tuple_filter(required(self.fields[direction])?, family, flags)?;
                }
            }
        }
        Ok(result)
    }
}
fn tuple_filter(bytes: &[u8], family: u8, flags: u32) -> Result<CtTupleFilter, SystemError> {
    if family != 2 && family != 10 {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    let tuple = attrs::<4>(bytes, &[(3, 2)], &[1, 2])?;
    let mut result = CtTupleFilter::default();
    if flags & 3 != 0 {
        let ip = attrs::<5>(
            required(tuple[1])?,
            &[(1, 4), (2, 4), (3, 16), (4, 16)],
            &[],
        )?;
        for (bit, v4, v6, output) in [(1, 1, 3, &mut result.src), (2, 2, 4, &mut result.dst)] {
            if flags & bit != 0 {
                *output = Some(if family == 2 {
                    CtAddress::V4(fixed(required(ip[v4])?)?)
                } else {
                    CtAddress::V6(fixed(required(ip[v6])?)?)
                });
            }
        }
    }
    if flags & 8 != 0 {
        let raw = required(tuple[2])?;
        let proto = attrs::<10>(raw, &[(1, 1)], &[])?;
        let number = fixed::<1>(required(proto[1])?)?[0];
        // The protocol-specific policy validates only its own fields; Linux
        // ignores attributes belonging to another transport protocol.
        let lengths: &[(u16, usize)] = match number {
            6 | 17 => &[(2, 2), (3, 2)],
            1 => &[(4, 2), (5, 1), (6, 1)],
            58 => &[(7, 2), (8, 1), (9, 1)],
            _ => &[],
        };
        let proto = attrs::<10>(raw, lengths, &[])?;
        result.protocol = Some(number);
        match number {
            6 | 17 => {
                if flags & (1 << 4) != 0 {
                    result.src_port = Some(u16::from_be_bytes(fixed(required(proto[2])?)?));
                }
                if flags & (1 << 5) != 0 {
                    result.dst_port = Some(u16::from_be_bytes(fixed(required(proto[3])?)?));
                }
            }
            1 | 58 => {
                let (id, kind, code, shift) = if number == 1 {
                    (4, 5, 6, 6)
                } else {
                    (7, 8, 9, 9)
                };
                if flags & (1 << shift) != 0 {
                    result.icmp_type = Some(fixed::<1>(required(proto[kind])?)?[0]);
                }
                if flags & (1 << (shift + 1)) != 0 {
                    result.icmp_code = Some(fixed::<1>(required(proto[code])?)?[0]);
                }
                if flags & (1 << (shift + 2)) != 0 {
                    result.icmp_id = Some(u16::from_be_bytes(fixed(required(proto[id])?)?));
                }
            }
            _ => {}
        }
    } else if flags & 0xff0 != 0 {
        return Err(SystemError::EINVAL);
    }
    if flags & 4 != 0 && tuple[3].is_some() {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    Ok(result)
}
