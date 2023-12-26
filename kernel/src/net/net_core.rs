use alloc::{boxed::Box, collections::BTreeMap, sync::Arc};
use smoltcp::{iface::SocketHandle, socket::dhcpv4, wire};
use system_error::SystemError;

use crate::{
    driver::net::NetDriver,
    kdebug, kinfo, kwarn,
    libs::rwlock::RwLockReadGuard,
    net::{socket::SocketPollMethod, NET_DRIVERS},
    time::timer::{next_n_ms_timer_jiffies, Timer, TimerFunction},
};

use super::{
    event_poll::EPollEventType,
    socket::{TcpSocket, HANDLE_MAP, SOCKET_SET},
};

/// The network poll function, which will be called by timer.
///
/// The main purpose of this function is to poll all network interfaces.
#[derive(Debug)]
struct NetWorkPollFunc;

impl TimerFunction for NetWorkPollFunc {
    fn run(&mut self) -> Result<(), SystemError> {
        poll_ifaces_try_lock(10).ok();
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
    let binding = NET_DRIVERS.write_irqsave();

    let net_face = binding.get(&0).ok_or(SystemError::ENODEV)?.clone();

    drop(binding);

    // Create sockets
    let mut dhcp_socket = dhcpv4::Socket::new();

    // Set a ridiculously short max lease time to show DHCP renews work properly.
    // This will cause the DHCP client to start renewing after 5 seconds, and give up the
    // lease after 10 seconds if renew hasn't succeeded.
    // IMPORTANT: This should be removed in production.
    dhcp_socket.set_max_lease_duration(Some(smoltcp::time::Duration::from_secs(10)));

    let dhcp_handle = SOCKET_SET.lock_irqsave().add(dhcp_socket);

    const DHCP_TRY_ROUND: u8 = 10;
    for i in 0..DHCP_TRY_ROUND {
        kdebug!("DHCP try round: {}", i);
        net_face.poll(&mut SOCKET_SET.lock_irqsave()).ok();
        let mut binding = SOCKET_SET.lock_irqsave();
        let event = binding.get_mut::<dhcpv4::Socket>(dhcp_handle).poll();

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
    let guard: RwLockReadGuard<BTreeMap<usize, Arc<dyn NetDriver>>> = NET_DRIVERS.read_irqsave();
    if guard.len() == 0 {
        kwarn!("poll_ifaces: No net driver found!");
        return;
    }
    let mut sockets = SOCKET_SET.lock_irqsave();
    for (_, iface) in guard.iter() {
        iface.poll(&mut sockets).ok();
    }
    let _ = send_event(&sockets);
}

/// 对ifaces进行轮询，最多对SOCKET_SET尝试times次加锁。
///
/// @return 轮询成功，返回Ok(())
/// @return 加锁超时，返回SystemError::EAGAIN_OR_EWOULDBLOCK
/// @return 没有网卡，返回SystemError::ENODEV
pub fn poll_ifaces_try_lock(times: u16) -> Result<(), SystemError> {
    let mut i = 0;
    while i < times {
        let guard: RwLockReadGuard<BTreeMap<usize, Arc<dyn NetDriver>>> =
            NET_DRIVERS.read_irqsave();
        if guard.len() == 0 {
            kwarn!("poll_ifaces: No net driver found!");
            // 没有网卡，返回错误
            return Err(SystemError::ENODEV);
        }
        let sockets = SOCKET_SET.try_lock_irqsave();
        // 加锁失败，继续尝试
        if sockets.is_err() {
            i += 1;
            continue;
        }

        let mut sockets = sockets.unwrap();
        for (_, iface) in guard.iter() {
            iface.poll(&mut sockets).ok();
        }
        let _ = send_event(&sockets);
        return Ok(());
    }

    // 尝试次数用完，返回错误
    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
}

/// 对ifaces进行轮询，最多对SOCKET_SET尝试一次加锁。
///
/// @return 轮询成功，返回Ok(())
/// @return 加锁超时，返回SystemError::EAGAIN_OR_EWOULDBLOCK
/// @return 没有网卡，返回SystemError::ENODEV
pub fn poll_ifaces_try_lock_onetime() -> Result<(), SystemError> {
    let guard: RwLockReadGuard<BTreeMap<usize, Arc<dyn NetDriver>>> = NET_DRIVERS.read_irqsave();
    if guard.len() == 0 {
        kwarn!("poll_ifaces: No net driver found!");
        // 没有网卡，返回错误
        return Err(SystemError::ENODEV);
    }
    let mut sockets = SOCKET_SET.try_lock_irqsave()?;
    for (_, iface) in guard.iter() {
        iface.poll(&mut sockets).ok();
    }
    send_event(&sockets)?;
    return Ok(());
}

/// ### 处理轮询后的事件
fn send_event(sockets: &smoltcp::iface::SocketSet) -> Result<(), SystemError> {
    for (handle, socket_type) in sockets.iter() {
        let handle_guard = HANDLE_MAP.read_irqsave();
        let item = handle_guard.get(&handle);
        if item.is_none() {
            continue;
        }

        let handle_item = item.unwrap();

        // 获取socket上的事件
        let mut events =
            SocketPollMethod::poll(socket_type, handle_item.shutdown_type()).bits() as u64;

        // 分发到相应类型socket处理
        match socket_type {
            smoltcp::socket::Socket::Raw(_) | smoltcp::socket::Socket::Udp(_) => {
                handle_guard
                    .get(&handle)
                    .unwrap()
                    .wait_queue
                    .wakeup_any(events);
            }
            smoltcp::socket::Socket::Icmp(_) => unimplemented!("Icmp socket hasn't unimplemented"),
            smoltcp::socket::Socket::Tcp(inner_socket) => {
                if inner_socket.is_active() {
                    events |= TcpSocket::CAN_ACCPET;
                }
                if inner_socket.state() == smoltcp::socket::tcp::State::Established {
                    events |= TcpSocket::CAN_CONNECT;
                }
                handle_guard
                    .get(&handle)
                    .unwrap()
                    .wait_queue
                    .wakeup_any(events);
            }
            smoltcp::socket::Socket::Dhcpv4(_) => {}
            smoltcp::socket::Socket::Dns(_) => unimplemented!("Dns socket hasn't unimplemented"),
        }
        drop(handle_guard);
        wakeup_epoll(handle, events as u32)?;
        // crate::kdebug!(
        //     "{} send_event {:?}",
        //     handle,
        //     EPollEventType::from_bits_truncate(events as u32)
        // );
    }
    Ok(())
}

/// ### 处理epoll
fn wakeup_epoll(handle: SocketHandle, events: u32) -> Result<(), SystemError> {
    let mut handle_guard = HANDLE_MAP.write_irqsave();
    let handle_item = handle_guard.get_mut(&handle).unwrap();
    let mut epitems_guard = handle_item.epitems.try_lock_irqsave()?;

    // 从events拿到epoll相关事件
    let pollflags = EPollEventType::from_bits_truncate(events);

    // 一次只取一个，因为一次也只有一个进程能拿到对应文件的🔓
    if let Some(epitem) = epitems_guard.pop_front() {
        let epoll = epitem.epoll().upgrade().unwrap();
        let mut epoll_guard = epoll.try_lock_irqsave()?;
        let binding = epitem.clone();
        let event_guard = binding.event().read_irqsave();
        let ep_events = EPollEventType::from_bits_truncate(event_guard.events());

        // 检查事件合理性以及是否有感兴趣的事件
        if !(ep_events
            .difference(EPollEventType::EP_PRIVATE_BITS)
            .is_empty()
            || pollflags.difference(ep_events).is_empty())
        {
            // TODO: 未处理pm相关

            // 首先将就绪的epitem加入等待队列
            epoll_guard.ep_add_ready(epitem.clone());

            if epoll_guard.ep_has_waiter() {
                if ep_events.contains(EPollEventType::EPOLLEXCLUSIVE)
                    && !pollflags.contains(EPollEventType::POLLFREE)
                {
                    // 避免惊群
                    epoll_guard.ep_wake_one();
                } else {
                    epoll_guard.ep_wake_all();
                }
            }
        }

        epitems_guard.push_back(epitem);
    }
    Ok(())
}
