use crossbeam_channel::{bounded, select, Receiver};
use pnet::packet::{
    icmp::{
        echo_reply::{EchoReplyPacket, IcmpCodes},
        echo_request::MutableEchoRequestPacket,
        IcmpTypes,
    },
    util, Packet,
};
use signal_hook::consts::{SIGINT, SIGTERM};
use socket2::{Domain, Protocol, Socket, Type};
use std::{
    io,
    net::{self, Ipv4Addr, SocketAddr},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    thread::{self},
    time::{Duration, Instant},
};

use crate::{config::Config, error::PingError};

#[derive(Clone)]
pub struct Ping {
    config: Config,
    socket: Arc<Socket>,
    dest: SocketAddr,
}

impl Ping {
    ///# ping创建函数
    /// 使用config进行ping的配置
    pub fn new(config: Config) -> std::io::Result<Self> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::ICMPV4))?;
        let src = SocketAddr::new(net::IpAddr::V4(Ipv4Addr::UNSPECIFIED), 12549);
        let dest = SocketAddr::new(config.address.ip, 12549);
        socket.bind(&src.into())?;
        // socket.set_ttl(64)?;
        // socket.set_read_timeout(Some(Duration::from_secs(config.timeout)))?;
        // socket.set_write_timeout(Some(Duration::from_secs(config.timeout)))?;
        Ok(Self {
            config,
            dest,
            socket: Arc::new(socket),
        })
    }
    ///# ping主要执行逻辑
    /// 创建icmpPacket发送给socket
    pub fn ping(&self, seq_offset: u16) -> anyhow::Result<()> {
        //创建 icmp request packet
        let mut buf = vec![0; self.config.packet_size];
        let mut icmp = MutableEchoRequestPacket::new(&mut buf[..]).expect("InvalidBuffferSize");
        icmp.set_icmp_type(IcmpTypes::EchoRequest);
        icmp.set_icmp_code(IcmpCodes::NoCode);
        icmp.set_identifier(self.config.id);
        icmp.set_sequence_number(self.config.sequence + seq_offset);
        icmp.set_checksum(util::checksum(icmp.packet(), 1));

        let start = Instant::now();

        //发送 request

        self.socket.send_to(icmp.packet(), &self.dest.into())?;

        //处理 recv
        let mut mem_buf =
            unsafe { &mut *(buf.as_mut_slice() as *mut [u8] as *mut [std::mem::MaybeUninit<u8>]) };
        let (size, _) = self.socket.recv_from(&mut mem_buf)?;

        let duration = start.elapsed().as_micros() as f64 / 1000.0;
        let reply = EchoReplyPacket::new(&buf).ok_or(PingError::InvalidPacket)?;
        println!(
            "{} bytes from {}: icmp_seq={} ttl={} time={:.2}ms",
            size,
            self.config.address.ip,
            reply.get_sequence_number(),
            self.config.ttl,
            duration
        );

        Ok(())
    }
    ///# ping指令多线程运行
    /// 创建多个线程负责不同的ping函数的执行
    pub fn run(&self) -> io::Result<()> {
        println!(
            "PING {}({})",
            self.config.address.raw, self.config.address.ip
        );
        let _now = Instant::now();
        let send = Arc::new(AtomicU64::new(0));
        let _send = send.clone();
        let this = Arc::new(self.clone());

        let success = Arc::new(AtomicU64::new(0));
        let _success = success.clone();

        let mut handles = vec![];

        for i in 0..this.config.count {
            let _this = this.clone();
            let handle = thread::spawn(move || {
                _this.ping(i).unwrap();
            });
            _send.fetch_add(1, Ordering::SeqCst);
            handles.push(handle);
            if i < this.config.count - 1 {
                thread::sleep(Duration::from_millis(this.config.interval));
            }
        }

        for handle in handles {
            if handle.join().is_ok() {
                _success.fetch_add(1, Ordering::SeqCst);
            }
        }

        let total = _now.elapsed().as_micros() as f64 / 1000.0;
        let send = send.load(Ordering::SeqCst);
        let success = success.load(Ordering::SeqCst);
        let loss_rate = if send > 0 {
            (send - success) * 100 / send
        } else {
            0
        };
        println!("\n--- {} ping statistics ---", self.config.address.raw);
        println!(
            "{} packets transmitted, {} received, {}% packet loss, time {}ms",
            send, success, loss_rate, total,
        );
        Ok(())
    }
}

//TODO: 等待添加ctrl+c发送信号后添加该特性
// /// # 创建一个进程用于监听用户是否提前退出程序
// fn signal_notify() -> std::io::Result<Receiver<i32>> {
//     let (s, r) = bounded(1);

//     let mut signals = signal_hook::iterator::Signals::new(&[SIGINT, SIGTERM])?;

//     thread::spawn(move || {
//         for signal in signals.forever() {
//             s.send(signal).unwrap();
//             break;
//         }
//     });
//     Ok(r)
// }
