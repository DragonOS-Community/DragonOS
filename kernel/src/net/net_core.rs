use alloc::{collections::BTreeMap, sync::Arc};
use log::{debug, info, warn};
use smoltcp::{socket::dhcpv4, wire};
use system_error::SystemError;

use crate::{
    driver::net::{Iface, Operstate},
    libs::rwlock::RwLockReadGuard,
    net::NET_DEVICES,
    time::{sleep::nanosleep, PosixTimeSpec},
};

pub fn net_init() -> Result<(), SystemError> {
    dhcp_query()
}

fn dhcp_query() -> Result<(), SystemError> {
    let binding = NET_DEVICES.write_irqsave();

    // Default iface, misspelled to net_face
    let net_face = binding
        .iter()
        .find(|(_, iface)| iface.common().is_default_iface())
        .unwrap()
        .1
        .clone();

    drop(binding);

    // Create sockets
    let mut dhcp_socket = dhcpv4::Socket::new();

    // Set a ridiculously short max lease time to show DHCP renews work properly.
    // This will cause the DHCP client to start renewing after 5 seconds, and give up the
    // lease after 10 seconds if renew hasn't succeeded.
    // IMPORTANT: This should be removed in production.
    dhcp_socket.set_max_lease_duration(Some(smoltcp::time::Duration::from_secs(10)));

    let sockets = || net_face.sockets().lock_irqsave();

    let dhcp_handle = sockets().add(dhcp_socket);
    defer::defer!({
        sockets().remove(dhcp_handle);
    });

    const DHCP_TRY_ROUND: u8 = 100;
    for i in 0..DHCP_TRY_ROUND {
        log::debug!("DHCP try round: {}", i);
        net_face.poll();
        let mut binding = sockets();
        let event = binding.get_mut::<dhcpv4::Socket>(dhcp_handle).poll();

        match event {
            None => {}

            Some(dhcpv4::Event::Configured(config)) => {
                // debug!("Find Config!! {config:?}");
                // debug!("Find ip address: {}", config.address);
                // debug!("iface.ip_addrs={:?}", net_face.inner_iface.ip_addrs());

                net_face
                    .update_ip_addrs(&[wire::IpCidr::Ipv4(config.address)])
                    .ok();

                if let Some(router) = config.router {
                    let mut smol_iface = net_face.smol_iface().lock();
                    smol_iface.routes_mut().update(|table| {
                        let _ = table.push(smoltcp::iface::Route {
                            cidr: smoltcp::wire::IpCidr::Ipv4(smoltcp::wire::Ipv4Cidr::new(
                                smoltcp::wire::Ipv4Address::new(127, 0, 0, 0),
                                8,
                            )),
                            via_router: smoltcp::wire::IpAddress::v4(127, 0, 0, 1),
                            preferred_until: None,
                            expires_at: None,
                        });
                    });
                    if smol_iface
                        .routes_mut()
                        .add_default_ipv4_route(router)
                        .is_err()
                    {
                        log::warn!("Route table full");
                    }
                    let cidr = smol_iface.ip_addrs().first().cloned();
                    if let Some(cidr) = cidr {
                        // 这里先在这里将网卡设置为up，后面等netlink实现了再修改
                        net_face.set_operstate(Operstate::IF_OPER_UP);
                        info!("Successfully allocated ip by Dhcpv4! Ip:{}", cidr);
                        return Ok(());
                    }
                } else {
                    net_face
                        .smol_iface()
                        .lock()
                        .routes_mut()
                        .remove_default_ipv4_route();
                }
            }

            Some(dhcpv4::Event::Deconfigured) => {
                debug!("Dhcp v4 deconfigured");
                net_face
                    .update_ip_addrs(&[smoltcp::wire::IpCidr::Ipv4(wire::Ipv4Cidr::new(
                        wire::Ipv4Address::UNSPECIFIED,
                        0,
                    ))])
                    .ok();
                net_face
                    .smol_iface()
                    .lock()
                    .routes_mut()
                    .remove_default_ipv4_route();
            }
        }
        // 在睡眠前释放锁
        drop(binding);

        let sleep_time = PosixTimeSpec {
            tv_sec: 0,
            tv_nsec: 50,
        };
        let _ = nanosleep(sleep_time)?;
    }

    return Err(SystemError::ETIMEDOUT);
}

pub fn poll_ifaces() {
    // log::debug!("poll_ifaces");
    let guard: RwLockReadGuard<BTreeMap<usize, Arc<dyn Iface>>> = NET_DEVICES.read_irqsave();
    if guard.len() == 0 {
        warn!("poll_ifaces: No net driver found!");
        return;
    }
    for (_, iface) in guard.iter() {
        iface.poll();
    }
}
