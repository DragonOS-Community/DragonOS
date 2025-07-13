// // src/main.rs
// use smoltcp::phy::{Device, DeviceCapabilities, RxToken, TxToken};
// use smoltcp::time::Instant;
// use std::collections::VecDeque;
// use std::sync::{Arc, Mutex};

// // 模拟 veth pair 中的一个端点
// pub struct VethInner {
//     queue: VecDeque<Vec<u8>>,
//     peer: Option<Arc<Mutex<VethInner>>>,
// }

// impl VethInner {
//     pub fn new() -> Self {
//         Self {
//             queue: VecDeque::new(),
//             peer: None,
//         }
//     }

//     pub fn set_peer(&mut self, peer: Arc<Mutex<VethInner>>) {
//         self.peer = Some(peer);
//     }

//     pub fn send_to_peer(&self, buf: Vec<u8>) {
//         if let Some(peer) = &self.peer {
//             peer.lock().unwrap().queue.push_back(buf);
//         }
//     }

//     pub fn recv(&mut self) -> Option<Vec<u8>> {
//         self.queue.pop_front()
//     }
// }

// #[derive(Clone)]
// pub struct VethDriver {
//     inner: Arc<Mutex<VethInner>>,
// }

// impl VethDriver {
//     pub fn new_pair() -> (Self, Self) {
//         let a = Arc::new(Mutex::new(VethInner::new()));
//         let b = Arc::new(Mutex::new(VethInner::new()));
//         a.lock().unwrap().set_peer(b.clone());
//         b.lock().unwrap().set_peer(a.clone());
//         (Self { inner: a }, Self { inner: b })
//     }
// }

// pub struct VethTxToken {
//     driver: VethDriver,
// }

// impl TxToken for VethTxToken {
//     fn consume<R, F>(self, len: usize, f: F) -> R
//     where
//         F: FnOnce(&mut [u8]) -> R,
//     {
//         let mut buffer = vec![0u8; len];
//         let result = f(&mut buffer);
//         self.driver.inner.lock().unwrap().send_to_peer(buffer);
//         result
//     }
// }

// pub struct VethRxToken {
//     buffer: Vec<u8>,
// }

// impl RxToken for VethRxToken {
//     fn consume<R, F>(self, f: F) -> R
//     where
//         F: FnOnce(&[u8]) -> R,
//     {
//         f(&self.buffer)
//     }
// }

// impl Device for VethDriver {
//     type RxToken<'a> = VethRxToken;
//     type TxToken<'a> = VethTxToken;

//     fn capabilities(&self) -> DeviceCapabilities {
//         let mut caps = DeviceCapabilities::default();
//         caps.max_transmission_unit = 1500;
//         caps.medium = smoltcp::phy::Medium::Ethernet;
//         caps
//     }

//     fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
//         let mut inner = self.inner.lock().unwrap();
//         if let Some(buf) = inner.recv() {
//             Some((
//                 VethRxToken { buffer: buf },
//                 VethTxToken {
//                     driver: self.clone(),
//                 },
//             ))
//         } else {
//             None
//         }
//     }

//     fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
//         Some(VethTxToken {
//             driver: self.clone(),
//         })
//     }
// }

// fn main() {
//     let (mut veth0, veth1) = VethDriver::new_pair();
//     let (veth3, mut veth4) = VethDriver::new_pair();

//     let mut bridge = BridgeDevice::new();
//     bridge.add_port(veth1.clone());
//     bridge.add_port(veth3.clone());

//     // veth0 → bridge → veth1 & veth3（→ veth4）
//     println!("--- veth0 → bridge (→ veth1, veth3) ---");
//     if let Some(tx) = veth0.transmit(Instant::from_millis(0)) {
//         tx.consume(32, |buf| {
//             buf[..6].copy_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]); // dst MAC
//             buf[6..12].copy_from_slice(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66]); // src MAC
//             buf[12..14].copy_from_slice(&(0x0800u16.to_be_bytes())); // Ethertype
//             buf[14..].copy_from_slice(b"hello bridge world"); // payload

//             bridge.handle_frame(&veth0, &buf);
//         });
//     }

//     if let Some((rx, _tx)) = veth1.clone().receive(Instant::from_millis(0)) {
//         rx.consume(|buf| {
//             println!("veth1 received: {:02x?}", buf);
//         });
//     } else {
//         println!("veth1 received nothing");
//     }

//     if let Some((rx, _tx)) = veth4.receive(Instant::from_millis(0)) {
//         rx.consume(|buf| {
//             println!("veth4 received: {:02x?}", buf);
//         });
//     } else {
//         println!("veth4 received nothing");
//     }
// }

// // 网桥设备：只做广播转发（无 MAC 学习）
// pub struct BridgeDevice {
//     pub ports: Vec<VethDriver>,
// }

// impl BridgeDevice {
//     pub fn new() -> Self {
//         BridgeDevice { ports: Vec::new() }
//     }

//     pub fn add_port(&mut self, port: VethDriver) {
//         self.ports.push(port);
//     }

//     pub fn remove_port(&mut self, port: &VethDriver) {
//         self.ports.retain(|p| !Arc::ptr_eq(&p.inner, &port.inner));
//     }

//     pub fn handle_frame(&mut self, src_if: &VethDriver, frame: &[u8]) {
//         for port in &self.ports {
//             if !Arc::ptr_eq(&port.inner, &src_if.inner) {
//                 port.inner.lock().unwrap().send_to_peer(frame.to_vec());
//             }
//         }
//     }
// }

// use std::net::UdpSocket;
// use std::str;
// use std::thread;
// use std::time::Duration;

// fn main() -> std::io::Result<()> {
//     // 启动 server 线程
//     let server_thread = thread::spawn(|| {
//         let socket =
//             UdpSocket::bind("10.0.0.2:34254").expect("Failed to bind to veth1 (10.0.0.2:34254)");
//         println!("[server] Listening on 10.0.0.2:34254");

//         let mut buf = [0; 1024];
//         let (amt, src) = socket
//             .recv_from(&mut buf)
//             .expect("[server] Failed to receive");

//         let received_msg = str::from_utf8(&buf[..amt]).expect("Invalid UTF-8");

//         println!("[server] Received from {}: {}", src, received_msg);

//         socket
//             .send_to(received_msg.as_bytes(), src)
//             .expect("[server] Failed to send back");
//         println!("[server] Echoed back the message");
//     });

//     // 确保 server 已启动（可根据情况适当 sleep）
//     thread::sleep(Duration::from_millis(200));

//     // 启动 client
//     let client_thread = thread::spawn(|| {
//         let socket = UdpSocket::bind("10.0.0.1:0").expect("Failed to bind to veth0 (10.0.0.1)");
//         socket
//             .connect("10.0.0.2:34254")
//             .expect("Failed to connect to 10.0.0.2:34254");

//         let msg = "Hello from veth0!";
//         socket
//             .send(msg.as_bytes())
//             .expect("[client] Failed to send");

//         println!("[client] Sent: {}", msg);

//         let mut buf = [0; 1024];
//         let (amt, _src) = socket
//             .recv_from(&mut buf)
//             .expect("[client] Failed to receive");

//         let received_msg = str::from_utf8(&buf[..amt]).expect("Invalid UTF-8");

//         println!("[client] Received echo: {}", received_msg);

//         assert_eq!(msg, received_msg, "[client] Mismatch in echo!");
//     });

//     // 等待两个线程结束
//     server_thread.join().unwrap();
//     client_thread.join().unwrap();

//     println!("\n✅ Test completed: veth0 <--> veth1 UDP communication success");

//     Ok(())
// }

//bridge

use std::net::UdpSocket;
use std::str;
use std::thread;
use std::time::Duration;

fn main() -> std::io::Result<()> {
    // 启动 server 线程
    let server_thread = thread::spawn(|| {
        let socket =
            UdpSocket::bind("200.0.0.4:34254").expect("Failed to bind to veth_d (200.0.0.4:34254)");
        println!("[server] Listening on 200.0.0.4:34254");

        let mut buf = [0; 1024];
        let (amt, src) = socket
            .recv_from(&mut buf)
            .expect("[server] Failed to receive");

        let received_msg = str::from_utf8(&buf[..amt]).expect("Invalid UTF-8");

        println!("[server] Received from {}: {}", src, received_msg);

        socket
            .send_to(received_msg.as_bytes(), src)
            .expect("[server] Failed to send back");
        println!("[server] Echoed back the message");
    });

    // 确保 server 已启动（可根据情况适当 sleep）
    thread::sleep(Duration::from_millis(200));

    // 启动 client
    let client_thread = thread::spawn(|| {
        let socket = UdpSocket::bind("200.0.0.1:0").expect("Failed to bind to veth_a (200.0.0.1)");
        socket
            .connect("200.0.0.4:34254")
            .expect("Failed to connect to 200.0.0.4:34254");

        let msg = "Hello from veth1!";
        socket
            .send(msg.as_bytes())
            .expect("[client] Failed to send");

        println!("[client] Sent: {}", msg);

        let mut buf = [0; 1024];
        let (amt, _src) = socket
            .recv_from(&mut buf)
            .expect("[client] Failed to receive");

        let received_msg = str::from_utf8(&buf[..amt]).expect("Invalid UTF-8");

        println!("[client] Received echo: {}", received_msg);

        assert_eq!(msg, received_msg, "[client] Mismatch in echo!");
    });

    // 等待两个线程结束
    server_thread.join().unwrap();
    client_thread.join().unwrap();

    println!("\n✅ Test completed: veth0 <--> veth1 UDP communication success");

    Ok(())
}
