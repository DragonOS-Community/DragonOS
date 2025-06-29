// src/main.rs
use smoltcp::phy::{Device, DeviceCapabilities, RxToken, TxToken};
use smoltcp::time::Instant;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

// 模拟 veth pair 中的一个端点
pub struct VethInner {
    queue: VecDeque<Vec<u8>>,
    peer: Option<Arc<Mutex<VethInner>>>,
}

impl VethInner {
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            peer: None,
        }
    }

    pub fn set_peer(&mut self, peer: Arc<Mutex<VethInner>>) {
        self.peer = Some(peer);
    }

    pub fn send_to_peer(&self, buf: Vec<u8>) {
        if let Some(peer) = &self.peer {
            peer.lock().unwrap().queue.push_back(buf);
        }
    }

    pub fn recv(&mut self) -> Option<Vec<u8>> {
        self.queue.pop_front()
    }
}

#[derive(Clone)]
pub struct VethDriver {
    inner: Arc<Mutex<VethInner>>,
}

impl VethDriver {
    pub fn new_pair() -> (Self, Self) {
        let a = Arc::new(Mutex::new(VethInner::new()));
        let b = Arc::new(Mutex::new(VethInner::new()));
        a.lock().unwrap().set_peer(b.clone());
        b.lock().unwrap().set_peer(a.clone());
        (Self { inner: a }, Self { inner: b })
    }
}

pub struct VethTxToken {
    driver: VethDriver,
}

impl TxToken for VethTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buffer = vec![0u8; len];
        let result = f(&mut buffer);
        self.driver.inner.lock().unwrap().send_to_peer(buffer);
        result
    }
}

pub struct VethRxToken {
    buffer: Vec<u8>,
}

impl RxToken for VethRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.buffer)
    }
}

impl Device for VethDriver {
    type RxToken<'a> = VethRxToken;
    type TxToken<'a> = VethTxToken;

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1500;
        caps.medium = smoltcp::phy::Medium::Ethernet;
        caps
    }

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(buf) = inner.recv() {
            Some((
                VethRxToken { buffer: buf },
                VethTxToken {
                    driver: self.clone(),
                },
            ))
        } else {
            None
        }
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(VethTxToken {
            driver: self.clone(),
        })
    }
}

// fn main() {
//     let (mut veth0, mut veth1) = VethDriver::new_pair();

//     // veth0 发，veth1 收
//     println!("--- veth0 → veth1 ---");
//     if let Some(tx) = veth0.transmit(Instant::from_millis(0)) {
//         tx.consume(32, |buf| {
//             buf[..6].copy_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
//             buf[6..12].copy_from_slice(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
//             buf[12..14].copy_from_slice(&(0x0800u16.to_be_bytes()));
//             buf[14..].copy_from_slice(b"hello veth1!      ");
//         });
//     }

//     if let Some((rx, _tx)) = veth1.receive(Instant::from_millis(0)) {
//         rx.consume(|buf| {
//             println!("veth1 received: {:02x?}", buf);
//         });
//     } else {
//         println!("veth1 received nothing");
//     }

//     // veth1 发，veth0 收
//     println!("--- veth1 → veth0 ---");
//     if let Some(tx) = veth1.transmit(Instant::from_millis(0)) {
//         tx.consume(28, |buf| {
//             buf[..6].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef, 0xde, 0xad]);
//             buf[6..12].copy_from_slice(&[0xca, 0xfe, 0xba, 0xbe, 0xca, 0xfe]);
//             buf[12..14].copy_from_slice(&(0x0806u16.to_be_bytes()));
//             buf[14..].copy_from_slice(b"yo veth0!     ");
//         });
//     }

//     if let Some((rx, _tx)) = veth0.receive(Instant::from_millis(0)) {
//         rx.consume(|buf| {
//             println!("veth0 received: {:02x?}", buf);
//         });
//     } else {
//         println!("veth0 received nothing");
//     }
// }

fn main() {
    let (mut veth0, veth1) = VethDriver::new_pair();
    let (veth3, mut veth4) = VethDriver::new_pair();

    let mut bridge = BridgeDevice::new();
    bridge.add_port(veth1.clone());
    bridge.add_port(veth3.clone());

    // veth0 → bridge → veth1 & veth3（→ veth4）
    println!("--- veth0 → bridge (→ veth1, veth3) ---");
    if let Some(tx) = veth0.transmit(Instant::from_millis(0)) {
        tx.consume(32, |buf| {
            buf[..6].copy_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]); // dst MAC
            buf[6..12].copy_from_slice(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66]); // src MAC
            buf[12..14].copy_from_slice(&(0x0800u16.to_be_bytes())); // Ethertype
            buf[14..].copy_from_slice(b"hello bridge world"); // payload

            bridge.handle_frame(&veth0, &buf);
        });
    }

    if let Some((rx, _tx)) = veth1.clone().receive(Instant::from_millis(0)) {
        rx.consume(|buf| {
            println!("veth1 received: {:02x?}", buf);
        });
    } else {
        println!("veth1 received nothing");
    }

    if let Some((rx, _tx)) = veth4.receive(Instant::from_millis(0)) {
        rx.consume(|buf| {
            println!("veth4 received: {:02x?}", buf);
        });
    } else {
        println!("veth4 received nothing");
    }
}

// 网桥设备：只做广播转发（无 MAC 学习）
pub struct BridgeDevice {
    pub ports: Vec<VethDriver>,
}

impl BridgeDevice {
    pub fn new() -> Self {
        BridgeDevice { ports: Vec::new() }
    }

    pub fn add_port(&mut self, port: VethDriver) {
        self.ports.push(port);
    }

    pub fn remove_port(&mut self, port: &VethDriver) {
        self.ports.retain(|p| !Arc::ptr_eq(&p.inner, &port.inner));
    }

    pub fn handle_frame(&mut self, src_if: &VethDriver, frame: &[u8]) {
        for port in &self.ports {
            if !Arc::ptr_eq(&port.inner, &src_if.inner) {
                port.inner.lock().unwrap().send_to_peer(frame.to_vec());
            }
        }
    }
}
