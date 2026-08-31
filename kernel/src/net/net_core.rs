use smoltcp::{socket::dhcpv4, wire};
use system_error::SystemError;

use crate::{
    driver::net::Operstate,
    net::address::{AddressMutation, AddressMutationOutcome},
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        namespace::net_namespace::INIT_NET_NAMESPACE,
    },
    time::{sleep::nanosleep, PosixTimeSpec},
};

enum DhcpControlEvent {
    Configured {
        address: wire::Ipv4Cidr,
        router: Option<wire::Ipv4Address>,
    },
    Deconfigured,
}

pub fn net_init() -> Result<(), SystemError> {
    KernelThreadMechanism::create_and_run(
        KernelThreadClosure::StaticEmptyClosure((&(dhcp_worker as fn() -> i32), ())),
        "dhcpv4".into(),
    )
    .ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)?;
    Ok(())
}

fn dhcp_worker() -> i32 {
    if let Err(err) = dhcp_query() {
        log::warn!("DHCP worker stopped without a lease: {err:?}");
    }
    0
}

fn dhcp_query() -> Result<(), SystemError> {
    // let binding = NET_DEVICES.write_irqsave();
    let binding = INIT_NET_NAMESPACE.device_list_mut();

    let net_face = binding
        .iter()
        .find(|(_, iface)| iface.name().starts_with("eth"))
        .map(|(_, iface)| iface.clone());

    if net_face.is_none() {
        log::warn!("dhcp_query: No net device found!");
        return Err(SystemError::ENODEV);
    }
    let net_face = net_face.unwrap();
    log::debug!("dhcp_query: net_face={}", net_face.name());
    drop(binding);

    // Create sockets
    let mut dhcp_socket = dhcpv4::Socket::new();

    // Set a ridiculously short max lease time to show DHCP renews work properly.
    // This will cause the DHCP client to start renewing after 5 seconds, and give up the
    // lease after 10 seconds if renew hasn't succeeded.
    // IMPORTANT: This should be removed in production.
    dhcp_socket.set_max_lease_duration(Some(smoltcp::time::Duration::from_secs(10)));

    let sockets = || net_face.sockets().lock();

    let dhcp_handle = sockets().add(dhcp_socket);
    defer::defer!({
        sockets().remove(dhcp_handle);
    });

    const DHCP_RETRY_INTERVAL_NS: i64 = 50_000_000;
    const DHCP_TRY_ROUND: u16 = 200;
    // Ownership is deliberately scoped to this one-shot worker invocation.
    // PR-08 owns the longer-lived lease state machine.
    let mut owned_address: Option<wire::IpCidr> = None;
    for i in 0..DHCP_TRY_ROUND {
        log::debug!("DHCP try round: {}", i);
        net_face.poll();
        let event = {
            let mut binding = sockets();
            binding
                .get_mut::<dhcpv4::Socket>(dhcp_handle)
                .poll()
                .map(|event| match event {
                    dhcpv4::Event::Configured(config) => DhcpControlEvent::Configured {
                        address: config.address,
                        router: config.router,
                    },
                    dhcpv4::Event::Deconfigured => DhcpControlEvent::Deconfigured,
                })
        };

        match event {
            None => {}

            Some(DhcpControlEvent::Configured { address, router }) => {
                // The DHCP socket guard is released before RTNL. Lease commits
                // share the same global serialization domain as userspace
                // rtnetlink mutations without extending it over DHCP polling.
                let rtnl_guard = crate::net::rtnl::lock();
                revalidate_dhcp_iface(&net_face)?;
                // debug!("Find Config!! {config:?}");
                // debug!("Find ip address: {}", config.address);
                // debug!("iface.ip_addrs={:?}", net_face.inner_iface.ip_addrs());

                let requested = wire::IpCidr::Ipv4(address);
                let mutation = match owned_address {
                    None => AddressMutation::Add(requested),
                    Some(old) if old == requested => AddressMutation::Replace(requested),
                    Some(old) => AddressMutation::ExchangeOwned {
                        old,
                        new: requested,
                    },
                };
                match crate::net::address::mutate_address(&rtnl_guard, &net_face, mutation) {
                    Ok(outcome) => {
                        owned_address = match outcome {
                            AddressMutationOutcome::Added(effective)
                            | AddressMutationOutcome::Replaced(effective) => Some(effective),
                            AddressMutationOutcome::Exchanged { new, .. } => Some(new),
                            AddressMutationOutcome::Deleted(_) => unreachable!(),
                        };
                        crate::net::socket::netlink::notify_address_outcome(
                            INIT_NET_NAMESPACE.clone(),
                            &net_face,
                            outcome,
                        );
                    }
                    Err(SystemError::EEXIST) if owned_address.is_none() => {
                        // A pre-existing address remains owned by its original
                        // writer. DHCP may use it for this invocation but must
                        // never delete or exchange it later.
                        log::debug!("DHCP address {requested} already configured");
                    }
                    Err(err) => return Err(err),
                }

                if let Some(router) = router {
                    let mut smol_iface = net_face.smol_iface().lock();
                    smol_iface.routes_mut().update(|table| {
                        let _ = table.push(smoltcp::iface::Route {
                            cidr: smoltcp::wire::IpCidr::Ipv4(smoltcp::wire::Ipv4Cidr::new(
                                smoltcp::wire::Ipv4Address::new(127, 0, 0, 0),
                                8,
                            )),
                            via_router: None,
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
                        log::info!("Successfully allocated ip by Dhcpv4! Ip:{}", cidr);
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

            Some(DhcpControlEvent::Deconfigured) => {
                let rtnl_guard = crate::net::rtnl::lock();
                revalidate_dhcp_iface(&net_face)?;
                log::debug!("Dhcp v4 deconfigured");
                if let Some(owned) = owned_address.take() {
                    match crate::net::address::mutate_address(
                        &rtnl_guard,
                        &net_face,
                        AddressMutation::Delete(owned),
                    ) {
                        Ok(outcome) => crate::net::socket::netlink::notify_address_outcome(
                            INIT_NET_NAMESPACE.clone(),
                            &net_face,
                            outcome,
                        ),
                        Err(SystemError::EADDRNOTAVAIL) => {
                            // A concurrent authorized control-plane writer may
                            // already have removed the object between events.
                            log::debug!("owned DHCP address {owned} was already removed");
                        }
                        Err(err) => return Err(err),
                    }
                }
                net_face
                    .smol_iface()
                    .lock()
                    .routes_mut()
                    .remove_default_ipv4_route();
            }
        }
        let sleep_time = PosixTimeSpec {
            tv_sec: 0,
            tv_nsec: DHCP_RETRY_INTERVAL_NS,
        };
        nanosleep(sleep_time)?;
    }

    return Err(SystemError::ETIMEDOUT);
}

fn revalidate_dhcp_iface(
    iface: &alloc::sync::Arc<dyn crate::driver::net::Iface>,
) -> Result<(), SystemError> {
    let devices = INIT_NET_NAMESPACE.device_list();
    let current = devices.get(&iface.nic_id()).ok_or(SystemError::ENODEV)?;
    if !alloc::sync::Arc::ptr_eq(current, iface) {
        return Err(SystemError::ENODEV);
    }
    Ok(())
}
