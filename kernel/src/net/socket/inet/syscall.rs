use system_error::SystemError::{self, *};
use smoltcp;

pub fn create_inet_socket(sock_type: crate::net::socket::define::Types, protocol: smoltcp::wire::IpProtocol) -> Result<super::InetSocket, SystemError> {
    use crate::net::socket::define::Types as SocketTypes;
    use smoltcp::wire::IpProtocol::*;
    match protocol {
        Udp => {
            if sock_type.types() != SocketTypes::DGRAM {
                return Err(EPROTONOSUPPORT);
            }
            todo!()
        }
        Tcp => {
            todo!()
        }
        Icmp => {
            todo!()
        }
        HopByHop => {
            if sock_type.types() != SocketTypes::RAW {
                return Err(EPROTONOSUPPORT);
            }
            todo!()
        }
        _ => {
            return Err(EPROTONOSUPPORT);
        }
    }
}