use smoltcp::{socket::dhcpv4, wire};

use crate::{kdebug, kinfo, net::NET_DRIVERS, syscall::SystemError};

pub fn net_init() -> Result<(), SystemError> {
    dhcp_query()?;
    return Ok(());
}
fn dhcp_query() -> Result<(), SystemError> {


    let binding = NET_DRIVERS.write();

    let net_face = binding.get(&0).unwrap().clone();

    drop(binding);

    // Create sockets
    let mut dhcp_socket = dhcpv4::Socket::new();

    // Set a ridiculously short max lease time to show DHCP renews work properly.
    // This will cause the DHCP client to start renewing after 5 seconds, and give up the
    // lease after 10 seconds if renew hasn't succeeded.
    // IMPORTANT: This should be removed in production.
    dhcp_socket.set_max_lease_duration(Some(smoltcp::time::Duration::from_secs(10)));

    let mut sockets = smoltcp::iface::SocketSet::new(vec![]);
    let dhcp_handle = sockets.add(dhcp_socket);

    const DHCP_TRY_ROUND: u8 = 10;
    for i in 0..DHCP_TRY_ROUND {
        kdebug!("DHCP try round: {}", i);
        let _flag = net_face.poll(&mut sockets);
        let event = sockets.get_mut::<dhcpv4::Socket>(dhcp_handle).poll();
        // kdebug!("event = {event:?} !!!");

        match event {
            None => {}

            Some(dhcpv4::Event::Configured(config)) => {
                // kdebug!("Find Config!! {config:?}");
                // kdebug!("Find ip address: {}", config.address);
                // kdebug!("iface.ip_addrs={:?}", net_face.inner_iface.ip_addrs());

                net_face
                    .update_ip_addrs(&[wire::IpCidr::Ipv4(config.address)])
                    .ok();

                if let Some(router) = config.router {
                    net_face
                        .inner_iface()
                        .lock()
                        .routes_mut()
                        .add_default_ipv4_route(router)
                        .unwrap();
                    let cidr = net_face.inner_iface().lock().ip_addrs().first().cloned();
                    if cidr.is_some() {
                        let cidr = cidr.unwrap();
                        kinfo!("Successfully allocated ip by Dhcpv4! Ip:{}", cidr);
                        return Ok(());
                    }
                } else {
                    net_face
                        .inner_iface()
                        .lock()
                        .routes_mut()
                        .remove_default_ipv4_route();
                }
            }

            Some(dhcpv4::Event::Deconfigured) => {
                kdebug!("Dhcp v4 deconfigured");
                net_face
                    .update_ip_addrs(&[smoltcp::wire::IpCidr::Ipv4(wire::Ipv4Cidr::new(
                        wire::Ipv4Address::UNSPECIFIED,
                        0,
                    ))])
                    .ok();
                net_face
                    .inner_iface()
                    .lock()
                    .routes_mut()
                    .remove_default_ipv4_route();
            }
        }
    }

    return Err(SystemError::ETIMEDOUT);
}
