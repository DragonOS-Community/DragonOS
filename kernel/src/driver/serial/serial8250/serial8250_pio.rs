//! PIO的串口驱动

use core::{
    hint::spin_loop,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use alloc::{
    collections::VecDeque,
    string::ToString,
    sync::{Arc, Weak},
};

use crate::{
    arch::{driver::apic::ioapic::IoApic, io::PortIOArch, CurrentIrqArch, CurrentPortIOArch},
    driver::{
        base::device::{
            device_number::{DeviceNumber, Major},
            DeviceId,
        },
        serial::{AtomicBaudRate, BaudRate, DivisorFraction, UartPort},
        tty::{
            console::ConsoleSwitch,
            kthread::{enqueue_tty_rx_byte_to_target_from_irq, TtyInputTarget},
            termios::{ControlMode, Termios, WindowSize},
            tty_core::{TtyCore, TtyCoreData},
            tty_driver::{TtyDriver, TtyDriverManager, TtyOperation},
            tty_port::TtyInputByteResult,
            virtual_terminal::{vc_manager, virtual_console::VirtualConsoleData, VirtConsole},
        },
        video::console::dummycon::dummy_console,
    },
    exception::{
        irqdata::IrqHandlerData,
        irqdesc::{IrqHandleFlags, IrqHandler, IrqReturn},
        manage::irq_manager,
        tasklet::{tasklet_schedule, Tasklet, TaskletData},
        InterruptArch, IrqNumber,
    },
    filesystem::epoll::{event_poll::EventPoll, EPollEventType},
    libs::{rwsem::RwSem, spinlock::SpinLock},
    process::ProcessManager,
    sched::sched_yield,
    time::{sleep::nanosleep, Duration, Instant, PosixTimeSpec},
};
use system_error::SystemError;

use super::{Serial8250ISADevices, Serial8250ISADriver, Serial8250Manager, Serial8250Port};

static mut PIO_PORTS: [Option<Serial8250PIOPort>; 8] =
    [None, None, None, None, None, None, None, None];

const SERIAL_8250_PIO_IRQS: [IrqNumber; 2] = [
    IrqNumber::new(IoApic::VECTOR_BASE as u32 + 3),
    IrqNumber::new(IoApic::VECTOR_BASE as u32 + 4),
];
const SERIAL_8250_IRQ_PASS_LIMIT: usize = 256;
const SERIAL_8250_RX_IRQ_LIMIT: usize = 256;
const SERIAL_8250_IER_RX_AVAILABLE: u8 = 0x01;
const SERIAL_8250_IER_TX_EMPTY: u8 = 0x02;
const SERIAL_8250_FCR_ENABLE_FIFO: u8 = 0x01;
const SERIAL_8250_FCR_CLEAR_XMIT: u8 = 0x04;
const SERIAL_8250_FCR_TRIGGER_14: u8 = 0xc0;
const SERIAL_8250_TX_QUEUE_SIZE: usize = 4096;
const SERIAL_8250_FIFO_SIZE: usize = 16;
const SERIAL_8250_TX_WAKEUP_CHARS: usize = 256;
const SERIAL_8250_EMERGENCY_TIMEOUT: Duration = Duration::from_millis(10);
const SERIAL_8250_CONSOLE_OWNER_NONE: usize = usize::MAX;
/// Linux tty_port_init() defaults closing_wait to 30 seconds. The software
/// queue and the physical transmitter share this single close-time budget.
const SERIAL_8250_CLOSE_WAIT: Duration = Duration::from_secs(30);

lazy_static! {
    static ref SERIAL_8250_RX_RETRY_TASKLET: Arc<Tasklet> =
        Tasklet::new(serial_8250_rx_retry_tasklet, 0, None);
}

pub(super) fn retry_serial8250_pio_input() {
    tasklet_schedule(&SERIAL_8250_RX_RETRY_TASKLET);
}

#[allow(static_mut_refs)]
fn serial_8250_rx_retry_tasklet(_data: usize, _data_obj: Option<Arc<dyn TaskletData>>) {
    for port in unsafe { PIO_PORTS.iter() }.flatten() {
        if port.rx_paused.load(Ordering::Acquire) {
            let _ = port.pump_rx();
        }
    }
}

impl Serial8250Manager {
    #[allow(static_mut_refs)]
    pub(super) fn bind_pio_ports(
        &self,
        uart_driver: &Arc<Serial8250ISADriver>,
        devs: &Arc<Serial8250ISADevices>,
    ) {
        for port in unsafe { &PIO_PORTS }.iter().flatten() {
            port.set_device(Some(devs));
            self.uart_add_one_port(uart_driver, port).ok();
        }
    }
}

macro_rules! init_port {
    ($port_num:expr) => {
        unsafe {
            let port = Serial8250PIOPort::new(
                match $port_num {
                    1 => Serial8250PortBase::COM1,
                    2 => Serial8250PortBase::COM2,
                    3 => Serial8250PortBase::COM3,
                    4 => Serial8250PortBase::COM4,
                    5 => Serial8250PortBase::COM5,
                    6 => Serial8250PortBase::COM6,
                    7 => Serial8250PortBase::COM7,
                    8 => Serial8250PortBase::COM8,
                    _ => panic!("invalid port number"),
                },
                crate::driver::serial::SERIAL_BAUDRATE,
            );
            if let Ok(port) = port {
                if port.init().is_ok() {
                    PIO_PORTS[$port_num - 1] = Some(port);
                    true
                } else {
                    false
                }
            } else {
                false
            }
        }
    };
}

/// 在内存管理初始化之前，初始化串口设备
pub(super) fn serial8250_pio_port_early_init() -> Result<(), SystemError> {
    for i in 1..=8 {
        init_port!(i);
    }

    return Ok(());
}

#[derive(Debug)]
pub struct Serial8250PIOPort {
    iobase: Serial8250PortBase,
    baudrate: AtomicBaudRate,
    frame_time_us: AtomicUsize,
    initialized: AtomicBool,
    tx_fifo_size: AtomicUsize,
    rx_paused: AtomicBool,
    rx_state: SpinLock<Serial8250RxState>,
    tx_state: SpinLock<Serial8250TxState>,
    /// Serializes the DLAB register-alias window against runtime UART I/O.
    hw_lock: SpinLock<()>,
    console_owner: AtomicUsize,
    inner: RwSem<Serial8250PIOPortInner>,
}

#[derive(Debug)]
struct Serial8250RxState {
    input_target: Option<TtyInputTarget>,
    ier: u8,
    irq_registered: bool,
    receiver_enabled: bool,
}

#[derive(Debug)]
struct Serial8250TxState {
    queue: Option<VecDeque<u8>>,
    tty: Weak<TtyCore>,
    console_active: bool,
    flush_generation: usize,
}

impl Serial8250TxState {
    fn new() -> Self {
        Self {
            // The PIO ports are constructed before the heap is available.
            // Allocate the process-context TX queue later during TTY install.
            queue: None,
            tty: Weak::new(),
            console_active: false,
            flush_generation: 0,
        }
    }
}

impl Serial8250RxState {
    const fn new() -> Self {
        Self {
            input_target: None,
            ier: 0,
            irq_registered: false,
            receiver_enabled: true,
        }
    }
}

impl Serial8250PIOPort {
    const SERIAL8250PIO_MAX_BAUD_RATE: BaudRate = BaudRate::new(115200);
    pub fn new(iobase: Serial8250PortBase, baudrate: BaudRate) -> Result<Self, SystemError> {
        let r = Self {
            iobase,
            baudrate: AtomicBaudRate::new(baudrate),
            // The early console starts in the conventional 8N1 format.
            frame_time_us: AtomicUsize::new(
                10_000_000usize.div_ceil(baudrate.data().max(1) as usize),
            ),
            initialized: AtomicBool::new(false),
            tx_fifo_size: AtomicUsize::new(1),
            rx_paused: AtomicBool::new(false),
            rx_state: SpinLock::new(Serial8250RxState::new()),
            tx_state: SpinLock::new(Serial8250TxState::new()),
            hw_lock: SpinLock::new(()),
            console_owner: AtomicUsize::new(SERIAL_8250_CONSOLE_OWNER_NONE),
            inner: RwSem::new(Serial8250PIOPortInner::new()),
        };

        r.check_baudrate(&baudrate)?;

        return Ok(r);
    }

    pub fn init(&self) -> Result<(), SystemError> {
        let r = self
            .initialized
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst);
        if r.is_err() {
            // 已经初始化
            return Ok(());
        }

        let port = self.iobase as u16;
        unsafe {
            CurrentPortIOArch::out8(port + 1, 0x00); // Disable all interrupts
            self.set_divisor(self.baudrate.load(Ordering::SeqCst))
                .unwrap(); // Set baud rate

            CurrentPortIOArch::out8(port + 2, 0xC7); // Enable FIFO, clear them, with 14-byte threshold
                                                     // IIR[7:6] == 11b is the 16550A FIFO-enabled indication.  Older
                                                     // 8250/16450-compatible hardware only guarantees one THR byte.
            let fifo_size = if CurrentPortIOArch::in8(port + 2) & 0xC0 == 0xC0 {
                SERIAL_8250_FIFO_SIZE
            } else {
                1
            };
            self.tx_fifo_size.store(fifo_size, Ordering::Release);
            CurrentPortIOArch::out8(port + 4, 0x08); // IRQs enabled, RTS/DSR clear (现代计算机上一般都不需要hardware flow control，因此不需要置位RTS/DSR)
            CurrentPortIOArch::out8(port + 4, 0x1E); // Set in loopback mode, test the serial chip
            CurrentPortIOArch::out8(port, 0xAE); // Test serial chip (send byte 0xAE and check if serial returns same byte)

            // Check if serial is faulty (i.e: not same byte as sent)
            if CurrentPortIOArch::in8(port) != 0xAE {
                self.initialized.store(false, Ordering::SeqCst);
                return Err(SystemError::ENODEV);
            }

            // If serial is not faulty set it in normal operation mode
            // (not-loopback with IRQs enabled and OUT#1 and OUT#2 bits enabled)
            CurrentPortIOArch::out8(port + 4, 0x0b);
        }
        return Ok(());
        /*
                Notice that the initialization code above writes to [PORT + 1]
            twice with different values. This is once to write to the Divisor
            register along with [PORT + 0] and once to write to the Interrupt
            register as detailed in the previous section.
                The second write to the Line Control register [PORT + 3]
            clears the DLAB again as well as setting various other bits.
        */
    }

    const fn check_baudrate(&self, baudrate: &BaudRate) -> Result<(), SystemError> {
        let baud = baudrate.data();
        if baud == 0
            || baud > Self::SERIAL8250PIO_MAX_BAUD_RATE.data()
            || Self::SERIAL8250PIO_MAX_BAUD_RATE.data().div_ceil(baud) > u16::MAX as u32
        {
            return Err(SystemError::EINVAL);
        }

        return Ok(());
    }

    #[allow(dead_code)]
    fn serial_received(&self) -> bool {
        self.serial_in(5) & 1 != 0
    }

    fn is_transmit_empty(&self) -> bool {
        self.serial_in(5) & 0x20 != 0
    }

    /// Both the transmitter FIFO and shift register are empty.
    fn is_transmitter_idle(&self) -> bool {
        self.serial_in(5) & 0x40 != 0
    }

    fn has_pending_interrupt(&self) -> bool {
        let _guard = self.hw_lock.lock_irqsave();
        self.serial_in_raw(2) & 0x01 == 0
    }

    fn irq(&self) -> Option<IrqNumber> {
        self.iobase.legacy_irq()
    }

    /// 读取一个字节，如果没有数据则返回None
    fn read_one_byte(&self) -> Option<u8> {
        let _guard = self.hw_lock.lock_irqsave();
        if self.serial_in_raw(5) & 1 == 0 {
            return None;
        }
        Some(self.serial_in_raw(0) as u8)
    }

    fn set_input_target(&self, target: Option<TtyInputTarget>) {
        self.rx_state.lock_irqsave().input_target = target;
    }

    fn set_rx_interrupt_enabled(&self, enabled: bool) {
        let mut rx_state = self.rx_state.lock_irqsave();
        self.set_rx_interrupt_enabled_locked(&mut rx_state, enabled);
    }

    fn set_tx_interrupt_enabled(&self, enabled: bool) {
        let mut rx_state = self.rx_state.lock_irqsave();
        if enabled {
            rx_state.ier |= SERIAL_8250_IER_TX_EMPTY;
        } else {
            rx_state.ier &= !SERIAL_8250_IER_TX_EMPTY;
        }
        self.sync_ier_locked(&rx_state);
    }

    fn enqueue_tx(&self, buf: &[u8]) -> usize {
        let accepted = {
            let mut tx_state = self.tx_state.lock_irqsave();
            let Some(queue) = tx_state.queue.as_mut() else {
                return 0;
            };
            let room = SERIAL_8250_TX_QUEUE_SIZE.saturating_sub(queue.len());
            let accepted = room.min(buf.len());
            queue.extend(&buf[..accepted]);
            accepted
        };
        if accepted != 0 {
            self.pump_tx();
        } else if self.tx_room() == 0 {
            self.set_tx_interrupt_enabled(true);
        }
        accepted
    }

    fn tx_room(&self) -> usize {
        self.tx_state
            .lock_irqsave()
            .queue
            .as_ref()
            .map(|queue| SERIAL_8250_TX_QUEUE_SIZE.saturating_sub(queue.len()))
            .unwrap_or(0)
    }

    fn tx_pending(&self) -> usize {
        self.tx_state
            .lock_irqsave()
            .queue
            .as_ref()
            .map(VecDeque::len)
            .unwrap_or(0)
    }

    fn pump_tx(&self) {
        let (sent, should_wake, tty) = {
            let mut tx_state = self.tx_state.lock_irqsave();
            if tx_state.console_active {
                return;
            }
            let tty = tx_state.tty.upgrade();
            let Some(queue) = tx_state.queue.as_mut() else {
                return;
            };
            let pending_before = queue.len();
            let mut sent = 0;
            // THRE must be sampled after taking tx_state: both process and
            // IRQ contexts can pump this port, and a pre-lock sample becomes
            // stale as soon as another context fills the FIFO.
            let _hw_guard = self.hw_lock.lock_irqsave();
            if self.serial_in_raw(5) & 0x20 != 0 {
                while sent < self.tx_fifo_size.load(Ordering::Acquire) {
                    let Some(byte) = queue.pop_front() else {
                        break;
                    };
                    self.serial_out_raw(0, byte.into());
                    sent += 1;
                }
            }
            drop(_hw_guard);
            let pending = !queue.is_empty();
            let pending_after = queue.len();
            let should_wake = pending_before != 0
                && (pending_after == 0
                    || (pending_before >= SERIAL_8250_TX_WAKEUP_CHARS
                        && pending_after < SERIAL_8250_TX_WAKEUP_CHARS));
            // Commit IER from the same queue snapshot before enqueue/clear can
            // mutate it, otherwise a stale disable can strand new bytes.
            self.set_tx_interrupt_enabled(pending);
            (sent, should_wake, tty)
        };

        if sent != 0 && should_wake {
            if let Some(tty) = tty {
                tty.tty_wakeup();
            }
        }
    }

    fn clear_tx(&self) {
        let mut tx_state = self.tx_state.lock_irqsave();
        if let Some(queue) = tx_state.queue.as_mut() {
            queue.clear();
        }
        tx_state.flush_generation = tx_state.flush_generation.wrapping_add(1);
        self.set_tx_interrupt_enabled(false);
    }

    fn abort_tx(&self) {
        let mut tx_state = self.tx_state.lock_irqsave();
        if let Some(queue) = tx_state.queue.as_mut() {
            queue.clear();
        }
        tx_state.flush_generation = tx_state.flush_generation.wrapping_add(1);
        self.set_tx_interrupt_enabled(false);

        // Linux's ordinary uart_flush_buffer() leaves bytes already accepted
        // by a PIO 8250 alone. A close-time abort is different: Linux follows
        // it with 8250 shutdown, which resets the hardware FIFOs before a
        // later open. DragonOS has no separate shutdown callback yet, so do
        // the transmit-only reset here without discarding pending RX data.
        if self.tx_fifo_size.load(Ordering::Acquire) > 1 {
            let _hw_guard = self.hw_lock.lock_irqsave();
            self.serial_out_raw(
                2,
                (SERIAL_8250_FCR_ENABLE_FIFO
                    | SERIAL_8250_FCR_CLEAR_XMIT
                    | SERIAL_8250_FCR_TRIGGER_14)
                    .into(),
            );
        }
    }

    fn write_fifo_if_ready(&self, bytes: &[u8]) -> usize {
        let _hw_guard = self.hw_lock.lock_irqsave();
        if self.serial_in_raw(5) & 0x20 == 0 {
            return 0;
        }
        let count = bytes.len().min(self.tx_fifo_size.load(Ordering::Acquire));
        for byte in &bytes[..count] {
            self.serial_out_raw(0, (*byte).into());
        }
        count
    }

    fn submit_runtime_output(&self, s: &[u8]) {
        // This is the legacy, no-return-value console/debug interface: unlike
        // the TTY write callback it cannot report a short write.  Serialize it
        // with the runtime queue, drain older TTY bytes first, then submit the
        // whole message by polling.  Console output is intentionally
        // synchronous; ordinary userspace TTY output remains IRQ-driven.
        let early_guard = self.tx_state.lock_irqsave();
        if early_guard.queue.is_none() {
            // Timers and the interrupt pipeline are not available yet.
            for byte in s {
                while !self.is_transmit_empty() {
                    spin_loop();
                }
                self.serial_out(0, (*byte).into());
            }
            return;
        }
        drop(early_guard);

        // A PCB address is stable across migration and is also visible from a
        // nested IRQ/exception on behalf of that task. Keep the Arc alive so
        // the token cannot be reused while this submission owns the UART.
        let current = ProcessManager::current_pcb();
        let owner_token = Arc::as_ptr(&current) as usize;
        if current.preempt_count() != 0 || !CurrentIrqArch::is_irq_enabled() {
            match self.console_owner.compare_exchange(
                SERIAL_8250_CONSOLE_OWNER_NONE,
                owner_token,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.submit_emergency_output(s);
                    self.console_owner
                        .store(SERIAL_8250_CONSOLE_OWNER_NONE, Ordering::Release);
                }
                Err(owner) if owner == owner_token => self.submit_emergency_output(s),
                Err(_) => {}
            }
            return;
        }
        loop {
            match self.console_owner.compare_exchange(
                SERIAL_8250_CONSOLE_OWNER_NONE,
                owner_token,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(owner) if owner == owner_token => {
                    self.submit_emergency_output(s);
                    return;
                }
                Err(_) => {
                    // Process-context callers may wait without pinning a CPU.
                    if nanosleep(PosixTimeSpec::new(0, 1_000_000)).is_err() {
                        return;
                    }
                }
            }
        }
        let (mut older_remaining, flush_generation, tty) = {
            let mut tx_state = self.tx_state.lock_irqsave();
            tx_state.console_active = true;
            let pending = tx_state.queue.as_ref().map(VecDeque::len).unwrap_or(0);
            let flush_generation = tx_state.flush_generation;
            let tty = tx_state.tty.upgrade();
            self.set_tx_interrupt_enabled(false);
            (pending, flush_generation, tty)
        };
        let timeout = self.tx_batch_timeout();
        let mut healthy = true;

        while older_remaining != 0 {
            if !self.wait_for_thre(timeout) {
                healthy = false;
                break;
            }
            let mut tx_state = self.tx_state.lock_irqsave();
            if tx_state.flush_generation != flush_generation {
                older_remaining = 0;
                break;
            }
            let Some(queue) = tx_state.queue.as_mut() else {
                older_remaining = 0;
                break;
            };
            let count = older_remaining
                .min(self.tx_fifo_size.load(Ordering::Acquire))
                .min(queue.len());
            if count == 0 {
                older_remaining = 0;
                break;
            }
            let _hw_guard = self.hw_lock.lock_irqsave();
            let sent = if self.serial_in_raw(5) & 0x20 == 0 {
                0
            } else {
                for _ in 0..count {
                    self.serial_out_raw(0, queue.pop_front().unwrap().into());
                }
                count
            };
            drop(_hw_guard);
            older_remaining -= sent;
            if sent == 0 {
                drop(tx_state);
                continue;
            }
        }

        let mut offset = 0;
        while healthy && offset < s.len() {
            if !self.wait_for_thre(timeout) {
                healthy = false;
                break;
            }
            let sent = self.write_fifo_if_ready(&s[offset..]);
            if sent == 0 {
                continue;
            }
            offset += sent;
        }

        {
            let mut tx_state = self.tx_state.lock_irqsave();
            tx_state.console_active = false;
            let pending = tx_state
                .queue
                .as_ref()
                .map(|queue| !queue.is_empty())
                .unwrap_or(false);
            self.set_tx_interrupt_enabled(pending);
        }
        if older_remaining == 0 {
            if let Some(tty) = tty {
                tty.tty_wakeup();
            }
        }
        if healthy {
            self.pump_tx();
        }
        self.console_owner
            .store(SERIAL_8250_CONSOLE_OWNER_NONE, Ordering::Release);
        drop(current);
    }

    fn submit_emergency_output(&self, s: &[u8]) {
        let Ok(mut tx_state) = self.tx_state.try_lock_irqsave() else {
            return;
        };
        let Ok(mut rx_state) = self.rx_state.try_lock_irqsave() else {
            return;
        };
        let Ok(_hw_guard) = self.hw_lock.try_lock_irqsave() else {
            return;
        };

        let inherited_console_active = tx_state.console_active;
        tx_state.console_active = true;
        rx_state.ier &= !SERIAL_8250_IER_TX_EMPTY;
        self.sync_ier_hw_locked(&rx_state);

        // Emergency output may run with interrupts or preemption disabled.
        // Keep this independent of the configured baud rate: at B50 the
        // ordinary FIFO-sized timeout would otherwise grow to several
        // seconds. Linux's 8250 console likewise bounds an LSR wait to 10 ms.
        let deadline = Instant::now() + SERIAL_8250_EMERGENCY_TIMEOUT;
        let mut ready = true;
        while self.serial_in_raw(5) & 0x20 == 0 {
            if Instant::now() >= deadline {
                ready = false;
                break;
            }
            spin_loop();
        }
        // Atomic diagnostics get one bounded FIFO batch. Longer output is
        // intentionally truncated rather than extending IRQ/exception time.
        if ready {
            let count = s.len().min(self.tx_fifo_size.load(Ordering::Acquire));
            for byte in &s[..count] {
                self.serial_out_raw(0, (*byte).into());
            }
        }

        tx_state.console_active = inherited_console_active;
        let pending = tx_state
            .queue
            .as_ref()
            .map(|queue| !queue.is_empty())
            .unwrap_or(false);
        if pending && !inherited_console_active {
            rx_state.ier |= SERIAL_8250_IER_TX_EMPTY;
        } else {
            rx_state.ier &= !SERIAL_8250_IER_TX_EMPTY;
        }
        self.sync_ier_hw_locked(&rx_state);
    }

    fn tx_batch_timeout(&self) -> Duration {
        let frame_time_us = self.frame_time_us.load(Ordering::Acquire) as u64;
        Duration::from_micros(
            (frame_time_us * self.tx_fifo_size.load(Ordering::Acquire) as u64 * 2).max(2_000),
        )
    }

    fn wait_for_thre(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while !self.is_transmit_empty() {
            if Instant::now() >= deadline {
                return false;
            }
            spin_loop();
        }
        true
    }

    fn wait_for_tx_queue_empty_until(
        &self,
        tty: &TtyCoreData,
        deadline: Instant,
    ) -> Result<bool, SystemError> {
        let remaining = deadline.saturating_sub(Instant::now());
        if remaining == Duration::ZERO {
            return Ok(self.tx_pending() == 0);
        }
        let events = (EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM).bits() as u64;
        tty.write_wq()
            .wait_event_interruptible_timeout(events, || self.tx_pending() == 0, remaining)
            .map(|_| true)
    }

    fn wait_for_transmitter_idle_until(&self, deadline: Instant) -> Result<bool, SystemError> {
        let poll_interval_us =
            (self.frame_time_us.load(Ordering::Acquire) as u64).clamp(1_000, 100_000);
        while !self.is_transmitter_idle() {
            let remaining_us = deadline.saturating_sub(Instant::now()).total_micros();
            if remaining_us == 0 {
                return Ok(false);
            }
            let sleep_us = poll_interval_us.min(remaining_us);
            nanosleep(PosixTimeSpec::new(0, sleep_us as i64 * 1_000))?;
        }
        Ok(true)
    }

    fn set_rx_interrupt_enabled_locked(&self, rx_state: &mut Serial8250RxState, enabled: bool) {
        if enabled {
            rx_state.ier |= SERIAL_8250_IER_RX_AVAILABLE;
        } else {
            rx_state.ier &= !SERIAL_8250_IER_RX_AVAILABLE;
        }
        self.sync_ier_locked(rx_state);
    }

    fn sync_ier_locked(&self, rx_state: &Serial8250RxState) {
        let _hw_guard = self.hw_lock.lock_irqsave();
        self.sync_ier_hw_locked(rx_state);
    }

    /// Synchronize IER while the caller holds `hw_lock`.
    fn sync_ier_hw_locked(&self, rx_state: &Serial8250RxState) {
        let ier = if rx_state.irq_registered {
            rx_state.ier
        } else {
            0
        };
        self.serial_out_raw(1, ier.into());
    }

    fn mark_rx_irq_registered(&self) {
        let mut rx_state = self.rx_state.lock_irqsave();
        rx_state.irq_registered = true;
        self.sync_ier_locked(&rx_state);
    }

    fn pause_rx_locked(&self, rx_state: &mut Serial8250RxState) {
        self.rx_paused.store(true, Ordering::Release);
        self.set_rx_interrupt_enabled_locked(rx_state, false);
    }

    fn resume_rx_locked(&self, rx_state: &mut Serial8250RxState) {
        self.rx_paused.store(false, Ordering::Release);
        self.set_rx_interrupt_enabled_locked(rx_state, true);
    }

    fn pump_rx(&self) -> Result<(), SystemError> {
        let mut rx_state = self.rx_state.lock_irqsave();
        let Some(target) = rx_state.input_target.clone() else {
            self.rx_paused.store(false, Ordering::Release);
            self.set_rx_interrupt_enabled_locked(&mut rx_state, false);
            return Ok(());
        };

        if !rx_state.receiver_enabled {
            for _ in 0..SERIAL_8250_RX_IRQ_LIMIT {
                if self.read_one_byte().is_none() {
                    break;
                }
            }
            self.rx_paused.store(false, Ordering::Release);
            self.set_rx_interrupt_enabled_locked(&mut rx_state, true);
            return Ok(());
        }

        // Linux serial8250_rx_chars also keeps a 256-byte bound to avoid
        // unbounded RX IRQ/softirq CPU occupation.
        let mut received = 0;
        for _ in 0..SERIAL_8250_RX_IRQ_LIMIT {
            let mut producer = || self.read_one_byte();
            match enqueue_tty_rx_byte_to_target_from_irq(&target, &mut producer) {
                TtyInputByteResult::Enqueued => {
                    received += 1;
                    self.rx_paused.store(false, Ordering::Release);
                }
                TtyInputByteResult::NoRoom => {
                    self.pause_rx_locked(&mut rx_state);
                    break;
                }
                TtyInputByteResult::NoData => {
                    self.resume_rx_locked(&mut rx_state);
                    break;
                }
            }
        }
        if received == SERIAL_8250_RX_IRQ_LIMIT {
            self.pause_rx_locked(&mut rx_state);
            retry_serial8250_pio_input();
        }
        Ok(())
    }

    fn set_tx_tty(&self, tty: Weak<TtyCore>) {
        let mut tx_state = self.tx_state.lock_irqsave();
        if tx_state.queue.is_none() {
            tx_state.queue = Some(VecDeque::with_capacity(SERIAL_8250_TX_QUEUE_SIZE));
        }
        tx_state.tty = tty;
    }

    fn line_control(control_mode: ControlMode) -> u8 {
        let mut lcr = match control_mode.intersection(ControlMode::CSIZE) {
            ControlMode::CS5 => 0,
            ControlMode::CS6 => 1,
            ControlMode::CS7 => 2,
            _ => 3,
        };
        if control_mode.contains(ControlMode::CSTOPB) {
            lcr |= 1 << 2;
        }
        if control_mode.contains(ControlMode::PARENB) {
            lcr |= 1 << 3;
            if !control_mode.contains(ControlMode::PARODD) {
                lcr |= 1 << 4;
            }
        }
        lcr
    }

    fn frame_time_us(control_mode: ControlMode, baudrate: BaudRate) -> usize {
        let data_bits = match control_mode.intersection(ControlMode::CSIZE) {
            ControlMode::CS5 => 5,
            ControlMode::CS6 => 6,
            ControlMode::CS7 => 7,
            _ => 8,
        };
        let parity_bits = usize::from(control_mode.contains(ControlMode::PARENB));
        let stop_bits = if control_mode.contains(ControlMode::CSTOPB) {
            2
        } else {
            1
        };
        let frame_bits = 1 + data_bits + parity_bits + stop_bits;
        (frame_bits * 1_000_000).div_ceil(baudrate.data().max(1) as usize)
    }

    fn baud_control_mode(baud: u32) -> Option<ControlMode> {
        Some(match baud {
            0 => ControlMode::B0,
            50 => ControlMode::B50,
            75 => ControlMode::B75,
            110 => ControlMode::B110,
            134 => ControlMode::B134,
            150 => ControlMode::B150,
            200 => ControlMode::B200,
            300 => ControlMode::B300,
            600 => ControlMode::B600,
            1200 => ControlMode::B1200,
            1800 => ControlMode::B1800,
            2400 => ControlMode::B2400,
            4800 => ControlMode::B4800,
            9600 => ControlMode::B9600,
            19200 => ControlMode::B19200,
            38400 => ControlMode::B38400,
            57600 => ControlMode::B57600,
            115200 => ControlMode::B115200,
            _ => return None,
        })
    }

    fn encode_baud_rate(termios: &mut Termios, baud: u32) {
        let Some(flag) = Self::baud_control_mode(baud) else {
            return;
        };
        let explicit_input_baud = termios.control_mode.intersects(ControlMode::CIBAUD);
        termios
            .control_mode
            .remove(ControlMode::CBAUD | ControlMode::CIBAUD);
        termios.control_mode.insert(flag);
        if explicit_input_baud {
            termios
                .control_mode
                .insert(ControlMode::from_bits_truncate(flag.bits() << 16));
        }
        termios.input_speed = baud;
        termios.output_speed = baud;
    }

    /// Apply the 8250 line settings as one register transaction.
    ///
    /// DLAB aliases offsets 0/1 with THR/RBR/IER. The established lock order
    /// is tx_state -> rx_state -> hw_lock, matching the TX and console paths.
    fn apply_line_settings(
        &self,
        control_mode: ControlMode,
        requested_baud: u32,
        fallback_baud: u32,
    ) -> Result<u32, SystemError> {
        let visible_baud = if requested_baud == 0 {
            0
        } else if requested_baud <= Self::SERIAL8250PIO_MAX_BAUD_RATE.data() {
            requested_baud
        } else {
            if fallback_baud <= Self::SERIAL8250PIO_MAX_BAUD_RATE.data() {
                fallback_baud
            } else {
                Self::SERIAL8250PIO_MAX_BAUD_RATE.data()
            }
        };
        // Linux programs a safe divisor while B0 controls hangup via MCR.
        let hardware_baud = BaudRate::new(if visible_baud == 0 {
            9600
        } else {
            visible_baud
        });
        self.check_baudrate(&hardware_baud)?;

        // The runtime console releases tx_state while polling but keeps
        // console_active set for the whole transaction. Emergency output
        // holds a fully try-locked tx_state/rx_state/hw_lock tuple instead.
        let _tx_state = loop {
            let guard = self.tx_state.lock_irqsave();
            if !guard.console_active {
                break guard;
            }
            drop(guard);
            sched_yield();
        };
        let mut rx_state = self.rx_state.lock_irqsave();
        rx_state.receiver_enabled = control_mode.contains(ControlMode::CREAD);
        let _hw_guard = self.hw_lock.lock_irqsave();
        let port = self.iobase as u16;
        let lcr = Self::line_control(control_mode);

        unsafe {
            // Stop UART interrupts before offset 1 becomes DLM.
            CurrentPortIOArch::out8(port + 1, 0);
            let divisor = self.divisor(hardware_baud).0;
            CurrentPortIOArch::out8(port + 3, lcr | 0x80);
            CurrentPortIOArch::out8(port, (divisor & 0xff) as u8);
            CurrentPortIOArch::out8(port + 1, ((divisor >> 8) & 0xff) as u8);
            CurrentPortIOArch::out8(port + 3, lcr);

            // Only B0 transitions change DTR/RTS; ordinary format changes
            // preserve independently managed modem-control state.
            let mcr = CurrentPortIOArch::in8(port + 4);
            let mcr = if fallback_baud != 0 && visible_baud == 0 {
                mcr & !0x03
            } else if fallback_baud == 0 && visible_baud != 0 {
                mcr | 0x03
            } else {
                mcr
            };
            CurrentPortIOArch::out8(port + 4, mcr);
            let ier = if rx_state.irq_registered {
                rx_state.ier
            } else {
                0
            };
            CurrentPortIOArch::out8(port + 1, ier);
        }

        self.baudrate.store(hardware_baud, Ordering::Release);
        self.frame_time_us.store(
            Self::frame_time_us(control_mode, hardware_baud),
            Ordering::Release,
        );
        Ok(visible_baud)
    }

    fn serial_in_raw(&self, offset: u32) -> u32 {
        unsafe { CurrentPortIOArch::in8(self.iobase as u16 + offset as u16).into() }
    }

    fn serial_out_raw(&self, offset: u32, value: u32) {
        unsafe { CurrentPortIOArch::out8(self.iobase as u16 + offset as u16, value as u8) }
    }
}

impl Serial8250Port for Serial8250PIOPort {
    fn device(&self) -> Option<Arc<Serial8250ISADevices>> {
        self.inner.read().device()
    }

    fn set_device(&self, device: Option<&Arc<Serial8250ISADevices>>) {
        self.inner.write().set_device(device);
    }
}

impl UartPort for Serial8250PIOPort {
    fn serial_in(&self, offset: u32) -> u32 {
        let _guard = self.hw_lock.lock_irqsave();
        self.serial_in_raw(offset)
    }

    fn serial_out(&self, offset: u32, value: u32) {
        // warning: pio的串口只能写入8位，因此这里丢弃高24位
        let _guard = self.hw_lock.lock_irqsave();
        self.serial_out_raw(offset, value)
    }

    fn divisor(&self, baud: BaudRate) -> (u32, DivisorFraction) {
        let divisor = (Self::SERIAL8250PIO_MAX_BAUD_RATE.data() + baud.data() / 2) / baud.data();
        return (divisor, DivisorFraction::new(0));
    }

    fn set_divisor(&self, baud: BaudRate) -> Result<(), SystemError> {
        self.check_baudrate(&baud)?;

        let port = self.iobase as u16;
        unsafe {
            CurrentPortIOArch::out8(port + 3, 0x80); // Enable DLAB (set baud rate divisor)

            let divisor = self.divisor(baud).0;

            CurrentPortIOArch::out8(port, (divisor & 0xff) as u8); // Set divisor  (lo byte)
            CurrentPortIOArch::out8(port + 1, ((divisor >> 8) & 0xff) as u8); // (hi byte)
            CurrentPortIOArch::out8(port + 3, 0x03); // 8 bits, no parity, one stop bit
        }

        self.baudrate.store(baud, Ordering::SeqCst);

        return Ok(());
    }

    fn startup(&self) -> Result<(), SystemError> {
        todo!("serial8250_pio::startup")
    }

    fn shutdown(&self) {
        todo!("serial8250_pio::shutdown")
    }

    fn baud_rate(&self) -> Option<BaudRate> {
        Some(self.baudrate.load(Ordering::SeqCst))
    }

    fn handle_irq(&self) -> Result<(), SystemError> {
        self.pump_rx()?;
        self.pump_tx();
        Ok(())
    }

    fn iobase(&self) -> Option<usize> {
        Some(self.iobase as usize)
    }
}

#[derive(Debug)]
struct Serial8250PIOPortInner {
    /// 当前端口绑定的设备
    ///
    /// ps: 存储weak以避免循环引用
    device: Option<Weak<Serial8250ISADevices>>,
}

impl Serial8250PIOPortInner {
    pub const fn new() -> Self {
        Self { device: None }
    }

    #[allow(dead_code)]
    pub fn device(&self) -> Option<Arc<Serial8250ISADevices>> {
        if let Some(device) = self.device.as_ref() {
            return device.upgrade();
        }
        return None;
    }

    fn set_device(&mut self, device: Option<&Arc<Serial8250ISADevices>>) {
        self.device = device.map(Arc::downgrade);
    }
}

#[allow(dead_code)]
#[repr(u16)]
#[derive(Clone, Debug, Copy)]
pub enum Serial8250PortBase {
    COM1 = 0x3f8,
    COM2 = 0x2f8,
    COM3 = 0x3e8,
    COM4 = 0x2e8,
    COM5 = 0x5f8,
    COM6 = 0x4f8,
    COM7 = 0x5e8,
    COM8 = 0x4e8,
}

impl Serial8250PortBase {
    /// Linux's fixed x86 legacy table defines IRQ resources only for the
    /// first four ISA UARTs. Additional base addresses require platform
    /// firmware or explicit configuration and must not guess an IRQ.
    const fn legacy_irq(self) -> Option<IrqNumber> {
        match self {
            Self::COM1 | Self::COM3 => Some(IrqNumber::new(IoApic::VECTOR_BASE as u32 + 4)),
            Self::COM2 | Self::COM4 => Some(IrqNumber::new(IoApic::VECTOR_BASE as u32 + 3)),
            Self::COM5 | Self::COM6 | Self::COM7 | Self::COM8 => None,
        }
    }
}

/// 临时函数，用于向COM1发送数据
pub fn send_to_default_serial8250_pio_port(s: &[u8]) {
    if let Some(port) = unsafe { PIO_PORTS[0].as_ref() } {
        port.submit_runtime_output(s);
    }
}

#[derive(Debug)]
pub(super) struct Serial8250PIOTtyDriverInner;

impl Serial8250PIOTtyDriverInner {
    pub fn new() -> Self {
        Self
    }

    fn do_install(
        &self,
        driver: Arc<TtyDriver>,
        tty: Arc<TtyCore>,
        vc: Arc<VirtConsole>,
    ) -> Result<(), SystemError> {
        driver.standard_install(tty.clone())?;
        vc.port().setup_internal_tty(Arc::downgrade(&tty));
        tty.set_port(vc.port());
        tty.core()
            .set_vc_index(vc.index().ok_or(SystemError::ENODEV)?);

        Ok(())
    }
}

impl TtyOperation for Serial8250PIOTtyDriverInner {
    fn open(&self, _tty: &TtyCoreData) -> Result<(), SystemError> {
        Ok(())
    }

    fn write(&self, tty: &TtyCoreData, buf: &[u8], nr: usize) -> Result<usize, SystemError> {
        let index = tty.index();
        if tty.index() >= unsafe { PIO_PORTS.len() } {
            return Err(SystemError::ENODEV);
        }
        let pio_port = unsafe { PIO_PORTS[index].as_ref() }.ok_or(SystemError::ENODEV)?;
        Ok(pio_port.enqueue_tx(&buf[..nr]))
    }

    fn write_room(&self, tty: &TtyCoreData) -> usize {
        let index = tty.index();
        unsafe { PIO_PORTS.get(index).and_then(Option::as_ref) }
            .map(Serial8250PIOPort::tx_room)
            .unwrap_or(0)
    }

    fn chars_in_buffer(&self, tty: &TtyCoreData) -> usize {
        let index = tty.index();
        unsafe { PIO_PORTS.get(index).and_then(Option::as_ref) }
            .map(Serial8250PIOPort::tx_pending)
            .unwrap_or(0)
    }

    fn flush_chars(&self, _tty: &TtyCoreData) {}

    fn wait_until_sent(&self, tty: &TtyCoreData) -> Result<(), SystemError> {
        let index = tty.index();
        if index >= unsafe { PIO_PORTS.len() } {
            return Err(SystemError::ENODEV);
        }
        let port = unsafe { PIO_PORTS[index].as_ref() }.ok_or(SystemError::ENODEV)?;
        // Linux bounds uart_wait_until_sent to twice the FIFO transmission
        // time when hardware flow control is disabled.
        let deadline = Instant::now() + port.tx_batch_timeout();
        port.wait_for_transmitter_idle_until(deadline)?;
        Ok(())
    }

    fn set_termios(&self, tty: Arc<TtyCore>, old_termios: Termios) -> Result<(), SystemError> {
        let index = tty.core().index();
        let port =
            unsafe { PIO_PORTS.get(index).and_then(Option::as_ref) }.ok_or(SystemError::ENODEV)?;
        let termios = *tty.core().termios();
        let applied_baud = port.apply_line_settings(
            termios.control_mode,
            termios.output_speed,
            old_termios.output_speed,
        )?;
        if !termios.control_mode.contains(ControlMode::CREAD) {
            // A full TTY input queue may have paused RX and masked its IRQ.
            // Drain already-received bytes now so CREAD-off data cannot be
            // delivered after the receiver is enabled again.
            port.pump_rx()?;
        }

        // Linux falls back to the previous supported baud when a request is
        // outside the hardware range. Keep the cached hardware fields aligned
        // with the divisor selected above. B0 intentionally remains zero.
        if applied_baud != termios.output_speed {
            let mut current = tty.core().termios_write();
            Serial8250PIOPort::encode_baud_rate(&mut current, applied_baud);
        }
        Ok(())
    }

    fn put_char(&self, tty: &TtyCoreData, ch: u8) -> Result<(), SystemError> {
        self.write(tty, &[ch], 1).map(|_| ())
    }

    fn ioctl(&self, _tty: Arc<TtyCore>, _cmd: u32, _arg: usize) -> Result<(), SystemError> {
        Err(SystemError::ENOIOCTLCMD)
    }

    fn close(&self, tty: Arc<TtyCore>) -> Result<(), SystemError> {
        let index = tty.core().index();
        let port =
            unsafe { PIO_PORTS.get(index).and_then(Option::as_ref) }.ok_or(SystemError::ENODEV)?;
        let close_deadline = Instant::now() + SERIAL_8250_CLOSE_WAIT;
        let queue_drained = port
            .wait_for_tx_queue_empty_until(tty.core(), close_deadline)
            .unwrap_or(false);
        let physical_deadline = close_deadline.min(Instant::now() + port.tx_batch_timeout());
        let transmitter_idle = queue_drained
            && port
                .wait_for_transmitter_idle_until(physical_deadline)
                .unwrap_or(false);
        if !transmitter_idle {
            // A failed/stalled UART must not leak bytes into the next open.
            port.abort_tx();
        }
        tty.ldisc().flush_buffer(tty.clone())?;
        Ok(())
    }

    fn resize(&self, tty: Arc<TtyCore>, winsize: WindowSize) -> Result<(), SystemError> {
        *tty.core().window_size_write() = winsize;
        Ok(())
    }

    fn install(&self, driver: Arc<TtyDriver>, tty: Arc<TtyCore>) -> Result<(), SystemError> {
        let tty_index = tty.core().index();
        if tty_index >= unsafe { PIO_PORTS.len() } {
            return Err(SystemError::ENODEV);
        }

        *tty.core().window_size_write() = WindowSize::DEFAULT;
        let vc_data = Arc::new(SpinLock::new(VirtualConsoleData::new(usize::MAX)));
        let mut vc_data_guard = vc_data.lock_irqsave();
        vc_data_guard.set_driver_funcs(Arc::downgrade(&dummy_console()) as Weak<dyn ConsoleSwitch>);
        vc_data_guard.init(
            Some(tty.core().window_size().row.into()),
            Some(tty.core().window_size().col.into()),
            true,
        );
        drop(vc_data_guard);
        let vc = VirtConsole::new(Some(vc_data));
        let port = unsafe { PIO_PORTS[tty_index].as_ref() }.ok_or(SystemError::ENODEV)?;
        let vc_index = vc_manager().alloc(vc.clone()).ok_or(SystemError::EBUSY)?;
        self.do_install(driver, tty.clone(), vc.clone())
            .inspect_err(|_| {
                vc_manager().free(vc_index);
                port.set_input_target(None);
                port.rx_paused.store(false, Ordering::Release);
                port.set_rx_interrupt_enabled(false);
            })?;
        port.set_tx_tty(Arc::downgrade(&tty));
        let irq_registered = {
            let mut rx_state = port.rx_state.lock_irqsave();
            rx_state.input_target = Some(TtyInputTarget::new(vc_index, vc.port()));
            port.pause_rx_locked(&mut rx_state);
            rx_state.irq_registered
        };
        if irq_registered {
            retry_serial8250_pio_input();
        }

        Ok(())
    }

    fn flush_buffer(&self, tty: &TtyCoreData) -> Result<(), SystemError> {
        let index = tty.index();
        let port =
            unsafe { PIO_PORTS.get(index).and_then(Option::as_ref) }.ok_or(SystemError::ENODEV)?;
        port.clear_tx();
        // Match Linux uart_flush_buffer(): releasing the software TX queue is
        // a write-readiness transition. Do not call tty_wakeup() here because
        // signal-character flush can enter with the N_TTY data lock held, and
        // its synchronous write_wakeup callback would re-enter that lock.
        tty.write_wq().wakeup_all();
        let events = EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM;
        let _ = EventPoll::wakeup_epoll(tty.epitems(), events);
        Ok(())
    }
}

pub(super) fn serial_8250_pio_register_tty_devices() -> Result<(), SystemError> {
    let (_, driver) = TtyDriverManager::lookup_tty_driver(DeviceNumber::new(
        Major::TTY_MAJOR,
        Serial8250Manager::TTY_SERIAL_MINOR_START,
    ))
    .ok_or(SystemError::ENODEV)?;

    let mut registered_irqs = [false; SERIAL_8250_PIO_IRQS.len()];
    for (slot, irq) in SERIAL_8250_PIO_IRQS.iter().copied().enumerate() {
        let has_port = unsafe { PIO_PORTS.iter() }
            .flatten()
            .any(|port| port.irq() == Some(irq));
        if !has_port {
            continue;
        }
        match irq_manager().request_irq(
            irq,
            "serial8250_pio".to_string(),
            &Serial8250IrqHandler,
            IrqHandleFlags::IRQF_SHARED | IrqHandleFlags::IRQF_TRIGGER_RISING,
            DeviceId::new(Some("serial8250_pio"), None),
        ) {
            Ok(()) => registered_irqs[slot] = true,
            Err(err) => {
                log::error!("failed to request serial 8250 PIO irq {:?}: {:?}", irq, err);
            }
        }
    }

    let irq_is_registered = |irq: IrqNumber| {
        SERIAL_8250_PIO_IRQS
            .iter()
            .position(|candidate| *candidate == irq)
            .is_some_and(|slot| registered_irqs[slot])
    };
    let mut registered_ports = 0;
    for (i, port) in unsafe { PIO_PORTS.iter() }.enumerate() {
        if let Some(port) = port {
            let Some(irq) = port.irq() else {
                log::warn!(
                    "serial 8250 PIO port {:?} has no enumerated IRQ resource; skipping tty registration",
                    port.iobase
                );
                continue;
            };
            if !irq_is_registered(irq) {
                log::error!(
                    "serial 8250 PIO port {:?} cannot be registered without IRQ {:?}",
                    port.iobase,
                    irq
                );
                continue;
            }
            let core = match driver.init_tty_device(Some(i)) {
                Ok(core) => core,
                Err(err) => {
                    log::error!(
                        "failed to init tty device for serial 8250 pio port {}, port iobase: {:?}: {:?}",
                        i,
                        port.iobase,
                        err
                    );
                    continue;
                }
            };
            if let Err(err) = core.resize(core.clone(), WindowSize::DEFAULT) {
                // The TTY already exists and cannot currently be rolled back.
                // Keep its IRQ route usable and retain the driver's default
                // size instead of abandoning this and every later port.
                log::error!(
                    "failed to resize tty device for serial 8250 pio port {}, port iobase: {:?}: {:?}",
                    i,
                    port.iobase,
                    err
                );
            }
            port.mark_rx_irq_registered();
            registered_ports += 1;
        }
    }

    if registered_ports == 0 && unsafe { PIO_PORTS.iter().any(Option::is_some) } {
        // free_irq() is not implemented yet. Once any request succeeded,
        // report this one-shot boot registration as complete instead of
        // inviting a retry that would duplicate the installed IRQ action.
        if registered_irqs.iter().any(|registered| *registered) {
            log::error!("no serial 8250 PIO TTY could be initialized");
            return Ok(());
        }
        return Err(SystemError::ENODEV);
    }
    retry_serial8250_pio_input();

    Ok(())
}

#[derive(Debug)]
struct Serial8250IrqHandler;

impl IrqHandler for Serial8250IrqHandler {
    fn handle(
        &self,
        irq: IrqNumber,
        _static_data: Option<&dyn IrqHandlerData>,
        _dynamic_data: Option<Arc<dyn IrqHandlerData>>,
    ) -> Result<IrqReturn, SystemError> {
        let mut handled = false;
        for _ in 0..SERIAL_8250_IRQ_PASS_LIMIT {
            let mut pending_in_pass = false;
            for port in unsafe { PIO_PORTS.iter() }
                .flatten()
                .filter(|port| port.irq() == Some(irq))
            {
                if !port.has_pending_interrupt() {
                    continue;
                }
                pending_in_pass = true;
                handled = true;
                if let Err(err) = port.handle_irq() {
                    log::error!(
                        "failed to service serial 8250 PIO port {:?} on irq {:?}: {:?}",
                        port.iobase,
                        irq,
                        err
                    );
                }
            }
            if !pending_in_pass {
                break;
            }
        }

        Ok(if handled {
            IrqReturn::Handled
        } else {
            IrqReturn::NotHandled
        })
    }
}
