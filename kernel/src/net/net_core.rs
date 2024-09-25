use alloc::{boxed::Box, collections::BTreeMap, sync::Arc};
use log::{debug, info, warn};
use smoltcp::{socket::dhcpv4, wire};
use system_error::SystemError;

use crate::libs::rwlock::RwLock;
use crate::{
    driver::net::Iface,
    libs::rwlock::RwLockReadGuard,
    net::NET_DEVICES,
    time::timer::{next_n_ms_timer_jiffies, Timer, TimerFunction},
};

use super::{
    event_poll::{EPollEventType, EventPoll},
    socket::{inet::TcpSocket, netlink::skbuff::SkBuff},
};

/// The network poll function, which will be called by timer.
///
/// The main purpose of this function is to poll all network interfaces.
#[derive(Debug)]
#[allow(dead_code)]
struct NetWorkPollFunc;

impl TimerFunction for NetWorkPollFunc {
    fn run(&mut self) -> Result<(), SystemError> {
        poll_ifaces();
        let next_time = next_n_ms_timer_jiffies(10);
        let timer = Timer::new(Box::new(NetWorkPollFunc), next_time);
        timer.activate();
        return Ok(());
    }
}

pub fn net_init() -> Result<(), SystemError> {
    dhcp_query()?;
    // Init poll timer function
    // let next_time = next_n_ms_timer_jiffies(5);
    // let timer = Timer::new(Box::new(NetWorkPollFunc), next_time);
    // timer.activate();
    return Ok(());
}

fn dhcp_query() -> Result<(), SystemError> {
    let binding = NET_DEVICES.write_irqsave();
    log::debug!("binding: {:?}", *binding);
    //由于现在os未实现在用户态为网卡动态分配内存，而lo网卡的id最先分配且ip固定不能被分配
    //所以特判取用id为0的网卡（也就是virto_net）
    let net_face = binding.get(&0).ok_or(SystemError::ENODEV)?.clone();

    drop(binding);

    // Create sockets
    let mut dhcp_socket = dhcpv4::Socket::new();

    // Set a ridiculously short max lease time to show DHCP renews work properly.
    // This will cause the DHCP client to start renewing after 5 seconds, and give up the
    // lease after 10 seconds if renew hasn't succeeded.
    // IMPORTANT: This should be removed in production.
    dhcp_socket.set_max_lease_duration(Some(smoltcp::time::Duration::from_secs(10)));

    let sockets = || net_face.sockets().lock_irqsave();

    // let dhcp_handle = SOCKET_SET.lock_irqsave().add(dhcp_socket);
    let dhcp_handle = sockets().add(dhcp_socket);

    const DHCP_TRY_ROUND: u8 = 10;
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
                    net_face
                        .smol_iface()
                        .lock()
                        .routes_mut()
                        .add_default_ipv4_route(router)
                        .unwrap();
                    let cidr = net_face.smol_iface().lock().ip_addrs().first().cloned();
                    if let Some(cidr) = cidr {
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
    }

    return Err(SystemError::ETIMEDOUT);
}

pub fn poll_ifaces() {
    log::debug!("poll_ifaces");
    let guard: RwLockReadGuard<BTreeMap<usize, Arc<dyn Iface>>> = NET_DEVICES.read_irqsave();
    if guard.len() == 0 {
        warn!("poll_ifaces: No net driver found!");
        return;
    }
    for (_, iface) in guard.iter() {
        iface.poll();
    }
}

// /// 对ifaces进行轮询，最多对SOCKET_SET尝试times次加锁。
// ///
// /// @return 轮询成功，返回Ok(())
// /// @return 加锁超时，返回SystemError::EAGAIN_OR_EWOULDBLOCK
// /// @return 没有网卡，返回SystemError::ENODEV
// pub fn poll_ifaces_try_lock(times: u16) -> Result<(), SystemError> {
//     let mut i = 0;
//     while i < times {
//         let guard: RwLockReadGuard<BTreeMap<usize, Arc<dyn Iface>>> =
//             NET_DEVICES.read_irqsave();
//         if guard.len() == 0 {
//             warn!("poll_ifaces: No net driver found!");
//             // 没有网卡，返回错误
//             return Err(SystemError::ENODEV);
//         }
//         for (_, iface) in guard.iter() {
//             iface.poll();
//         }
//         return Ok(());
//     }
//     // 尝试次数用完，返回错误
//     return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
// }

// /// 对ifaces进行轮询，最多对SOCKET_SET尝试一次加锁。
// ///
// /// @return 轮询成功，返回Ok(())
// /// @return 加锁超时，返回SystemError::EAGAIN_OR_EWOULDBLOCK
// /// @return 没有网卡，返回SystemError::ENODEV
// pub fn poll_ifaces_try_lock_onetime() -> Result<(), SystemError> {
//     let guard: RwLockReadGuard<BTreeMap<usize, Arc<dyn Iface>>> = NET_DEVICES.read_irqsave();
//     if guard.len() == 0 {
//         warn!("poll_ifaces: No net driver found!");
//         // 没有网卡，返回错误
//         return Err(SystemError::ENODEV);
//     }
//     for (_, iface) in guard.iter() {
//         let _ = iface.poll();
//     }
//     send_event()?;
//     return Ok(());
// }

// /// ### 处理轮询后的事件
// fn send_event() -> Result<(), SystemError> {
//     for (handle, socket_type) in .lock().iter() {

//         let global_handle = GlobalSocketHandle::new_smoltcp_handle(handle);

//         let handle_guard = HANDLE_MAP.read_irqsave();
//         let item: Option<&super::socket::SocketHandleItem> = handle_guard.get(&global_handle);
//         if item.is_none() {
//             continue;
//         }

//         let handle_item = item.unwrap();
//         let posix_item = handle_item.posix_item();
//         if posix_item.is_none() {
//             continue;
//         }
//         let posix_item = posix_item.unwrap();

//         // 获取socket上的事件
//         let mut events = SocketPollMethod::poll(socket_type, handle_item).bits() as u64;

//         // 分发到相应类型socket处理
//         match socket_type {
//             smoltcp::socket::Socket::Raw(_) | smoltcp::socket::Socket::Udp(_) => {
//                 posix_item.wakeup_any(events);
//             }
//             smoltcp::socket::Socket::Icmp(_) => unimplemented!("Icmp socket hasn't unimplemented"),
//             smoltcp::socket::Socket::Tcp(inner_socket) => {
//                 if inner_socket.is_active() {
//                     events |= TcpSocket::CAN_ACCPET;
//                 }
//                 if inner_socket.state() == smoltcp::socket::tcp::State::Established {
//                     events |= TcpSocket::CAN_CONNECT;
//                 }
//                 if inner_socket.state() == smoltcp::socket::tcp::State::CloseWait {
//                     events |= EPollEventType::EPOLLHUP.bits() as u64;
//                 }

//                 posix_item.wakeup_any(events);
//             }
//             smoltcp::socket::Socket::Dhcpv4(_) => {}
//             smoltcp::socket::Socket::Dns(_) => unimplemented!("Dns socket hasn't unimplemented"),
//         }
//         EventPoll::wakeup_epoll(
//             &posix_item.epitems,
//             EPollEventType::from_bits_truncate(events as u32),
//         )?;
//         drop(handle_guard);
//     }
//     Ok(())
// }
