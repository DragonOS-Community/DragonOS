use alloc::{boxed::Box, collections::BTreeMap, sync::Arc};
use smoltcp::{socket::dhcpv4, wire};

use crate::{
    driver::net::NetDriver,
    kdebug, kinfo, kwarn,
    libs::rwlock::RwLockReadGuard,
    net::NET_DRIVERS,
    syscall::SystemError,
    time::timer::{next_n_ms_timer_jiffies, Timer, TimerFunction},
};

use super::socket::{SOCKET_SET, SOCKET_WAITQUEUE};

/// The network poll function, which will be called by timer.
///
/// The main purpose of this function is to poll all network interfaces.
struct NetWorkPollFunc();
impl TimerFunction for NetWorkPollFunc {
    fn run(&mut self) {
        poll_ifaces_try_lock(10).ok();
        let next_time = next_n_ms_timer_jiffies(10);
        let timer = Timer::new(Box::new(NetWorkPollFunc()), next_time);
        timer.activate();
    }
}

pub fn net_init() -> Result<(), SystemError> {
    dhcp_query()?;
    // Init poll timer function
    let next_time = next_n_ms_timer_jiffies(5);
    let timer = Timer::new(Box::new(NetWorkPollFunc()), next_time);
    timer.activate();
    return Ok(());
}
fn dhcp_query() -> Result<(), SystemError> {
    let binding = NET_DRIVERS.write();

    let net_face = binding.get(&0).ok_or(SystemError::ENODEV)?.clone();

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

pub fn poll_ifaces() {
    let guard: RwLockReadGuard<BTreeMap<usize, Arc<dyn NetDriver>>> = NET_DRIVERS.read();
    if guard.len() == 0 {
        kwarn!("poll_ifaces: No net driver found!");
        return;
    }
    let mut sockets = SOCKET_SET.lock();
    for (_, iface) in guard.iter() {
        iface.poll(&mut sockets).ok();
    }
    SOCKET_WAITQUEUE.wakeup_all((-1i64) as u64);
}

/// 对ifaces进行轮询，最多对SOCKET_SET尝试times次加锁。
///
/// @return 轮询成功，返回Ok(())
/// @return 加锁超时，返回SystemError::EAGAIN_OR_EWOULDBLOCK
/// @return 没有网卡，返回SystemError::ENODEV
pub fn poll_ifaces_try_lock(times: u16) -> Result<(), SystemError> {
    let mut i = 0;
    while i < times {
        let guard: RwLockReadGuard<BTreeMap<usize, Arc<dyn NetDriver>>> = NET_DRIVERS.read();
        if guard.len() == 0 {
            kwarn!("poll_ifaces: No net driver found!");
            // 没有网卡，返回错误
            return Err(SystemError::ENODEV);
        }
        let sockets = SOCKET_SET.try_lock();
        // 加锁失败，继续尝试
        if sockets.is_err() {
            i += 1;
            continue;
        }

        let mut sockets = sockets.unwrap();
        for (_, iface) in guard.iter() {
            iface.poll(&mut sockets).ok();
        }
        SOCKET_WAITQUEUE.wakeup_all((-1i64) as u64);
        return Ok(());
    }

    // 尝试次数用完，返回错误
    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
}
