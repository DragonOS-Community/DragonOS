use core::ops::DerefMut;

use alloc::sync::Arc;
use smoltcp::{socket::dhcpv4, wire};

use crate::{
    driver::{
        net::{virtio_net::VirtioNICDriver, NetDriver},
        virtio::transport_pci::PciTransport,
        NET_DRIVERS,
    },
    kdebug, kinfo,
    net::NET_FACES,
    syscall::SystemError,
    time::{timekeep::ktime_get_real_ns, timer::schedule_timeout},
};

use super::Interface;

pub fn net_init() -> Result<(), SystemError> {
    dhcp_query()?;
    return Ok(());
}
fn dhcp_query() -> Result<(), SystemError> {
    // let mut binding = NET_DRIVERS.write();

    // let device = unsafe {
    //     (binding.get(&0).unwrap().as_ref() as *const dyn NetDriver
    //         as *const VirtioNICDriver<PciTransport> as *mut VirtioNICDriver<PciTransport>)
    //         .as_mut()
    //         .unwrap()
    // };

    let binding = NET_FACES.write();

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
        kdebug!("poll status = {:?}", _flag);
        let event = sockets.get_mut::<dhcpv4::Socket>(dhcp_handle).poll();
        // kdebug!("event = {event:?} !!!");

        match event {
            None => {}

            Some(dhcpv4::Event::Configured(config)) => {
                // kdebug!("Find Config!! {config:?}");
                // kdebug!("Find ip address: {}", config.address);
                // kdebug!("iface.ip_addrs={:?}", net_face.inner_iface.ip_addrs());
                set_ipv4_addr(&net_face, config.address);
                if let Some(router) = config.router {
                    net_face
                        .inner_iface
                        .write()
                        .routes_mut()
                        .add_default_ipv4_route(router)
                        .unwrap();
                    let cidr = net_face.inner_iface.write().ip_addrs().first().cloned();
                    if cidr.is_some() {
                        let cidr = cidr.unwrap();
                        kinfo!("Successfully allocated ip by Dhcpv4! Ip:{}", cidr,)
                    }
                    break;
                } else {
                    net_face
                        .inner_iface
                        .write()
                        .routes_mut()
                        .remove_default_ipv4_route();
                }
            }

            Some(dhcpv4::Event::Deconfigured) => {
                kdebug!("deconfigured");
                set_ipv4_addr(
                    &net_face,
                    wire::Ipv4Cidr::new(wire::Ipv4Address::UNSPECIFIED, 0),
                );
                net_face
                    .inner_iface
                    .write()
                    .routes_mut()
                    .remove_default_ipv4_route();
            }
        }
    }

    return Ok(());
}

fn set_ipv4_addr(iface: &Arc<Interface>, cidr: wire::Ipv4Cidr) {
    // kdebug!("set cidr = {cidr:?}");

    iface.inner_iface.write().update_ip_addrs(|addrs| {
        let dest = addrs.iter_mut().next();
        if let None = dest {
            addrs
                .push(wire::IpCidr::Ipv4(cidr))
                .expect("Push ipCidr failed: full");
        } else {
            let dest = dest.unwrap();
            *dest = wire::IpCidr::Ipv4(cidr);
        }
    });
}
