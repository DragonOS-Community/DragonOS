use std::{
    fs::File,
    mem::size_of,
    os::unix::prelude::FileExt,
    path::PathBuf,
    sync::{atomic::AtomicBool, mpsc, Arc, Mutex, Weak},
    thread::JoinHandle,
};

use klog_types::{AllocatorLog, MMLogChannel};
use log::info;

use crate::backend::{
    loader::Symbol,
    monitor::{logset::LogSet, ObjectWrapper},
    BackendData,
};

#[derive(Debug)]
pub struct MMLogMonitor {
    channel_symbol: Option<Symbol>,
    shared_data: Arc<Mutex<BackendData>>,
    /// All threads spawned by the mm log monitor.
    threads: Mutex<Vec<JoinHandle<()>>>,
    stop_child_threads: AtomicBool,
    self_ref: Weak<Self>,

    mm_log_receiver: Mutex<mpsc::Receiver<MMLogWorkerResult>>,
    mm_log_sender: mpsc::Sender<MMLogWorkerResult>,
}

impl MMLogMonitor {
    pub fn new(shared_data: Arc<Mutex<BackendData>>) -> Arc<Self> {
        let guard = shared_data.lock().unwrap();
        let mm_log_buffer_symbol: Option<Symbol> = guard
            .kernel_metadata
            .as_ref()
            .map(|km| {
                km.sym_collection()
                    .find_by_name("__MM_ALLOCATOR_LOG_CHANNEL")
                    .map(|s| s.clone())
            })
            .flatten();
        drop(guard);

        info!("mm_log_buffer_symbol: {:?}", mm_log_buffer_symbol);

        let mm_log_worker_mpsc: (
            mpsc::Sender<MMLogWorkerResult>,
            mpsc::Receiver<MMLogWorkerResult>,
        ) = mpsc::channel::<MMLogWorkerResult>();

        let r = Self {
            channel_symbol: mm_log_buffer_symbol,
            shared_data,
            threads: Mutex::new(Vec::new()),
            stop_child_threads: AtomicBool::new(false),
            self_ref: Weak::new(),
            mm_log_receiver: Mutex::new(mm_log_worker_mpsc.1),
            mm_log_sender: mm_log_worker_mpsc.0,
        };

        let r = Arc::new(r);
        unsafe {
            let self_ref = Arc::downgrade(&r);
            let r_ptr = r.as_ref() as *const Self as *mut Self;
            (*r_ptr).self_ref = self_ref;
        }

        return r;
    }

    pub fn run(&self) {
        info!("MMLogMonitor::run()");

        self.create_threads();

        let mut logs_set =
            LogSet::<usize, ObjectWrapper<AllocatorLog>>::new("mm_allocator_log".to_string(), None);

        self.handle_logs(&mut logs_set);
        // std::thread::sleep(std::time::Duration::from_micros(50));
    }

    fn handle_logs(&self, logs_set: &mut LogSet<usize, ObjectWrapper<AllocatorLog>>) {
        let mut last_cnt = 0;
        let mut last_time = std::time::Instant::now();
        let mm_log_receiver = self.mm_log_receiver.lock().unwrap();
        loop {
            let logs = mm_log_receiver.recv();
            if logs.is_err() {
                return;
            }

            let logs = logs.unwrap();

            for log in logs.logs {
                logs_set.insert(log.id as usize, log);
            }

            let x = logs_set.len();
            // info!("logs_set.len(): {}", x);
            let current_time = std::time::Instant::now();
            if current_time.duration_since(last_time).as_secs() >= 1 {
                info!("memory log rate: {} logs/s", x - last_cnt);
                last_cnt = x;
                last_time = current_time;
            }
        }
    }

    // fn show_speed(&self, )

    fn create_threads(&self) {
        let km = self
            .shared_data
            .lock()
            .unwrap()
            .kmem_path
            .clone()
            .expect("DragonOS memory map file not specified.");
        let monitor_weak = self.self_ref.clone();

        let handle = std::thread::spawn(move || {
            let mut monitor_thread = MMMonitorThread::new(monitor_weak, PathBuf::from(km));
            monitor_thread.run();
        });

        self.threads.lock().unwrap().push(handle);
    }
}

#[derive(Debug)]
struct MMMonitorThread {
    mm_log_monitor: Weak<MMLogMonitor>,
    kmem_path: PathBuf,
}

impl MMMonitorThread {
    /// Constructs a new instance of [`MMMonitorThread`].
    ///
    /// ## Parameters
    ///
    /// - `mm_log_monitor`: The [`MMLogMonitor`] instance.
    /// - `kmem_path`: The path to the kernel memory file.
    pub fn new(mm_log_monitor: Weak<MMLogMonitor>, kmem_path: PathBuf) -> Self {
        Self {
            mm_log_monitor,
            kmem_path,
        }
    }

    pub fn run(&mut self) {
        info!("MMMonitorThread::run(): kmem_path: {:?}", self.kmem_path);

        let mut kmem_file = self.open_kmem_file().expect("Failed to open kmem file.");

        info!("Channel header loaded!");

        let channel_header: ObjectWrapper<MMLogChannel<1>> = self.load_header(&mut kmem_file);

        // 循环读取

        self.process_logs(&mut kmem_file, &channel_header);
    }

    /// 处理内核内存分配日志
    fn process_logs(&self, kmem_file: &mut File, channel_header: &ObjectWrapper<MMLogChannel<1>>) {
        let cap = channel_header.capacity;
        let mut buf = vec![0u8; (cap * channel_header.slot_size as u64) as usize];
        let symbol = self
            .mm_log_channel_symbol()
            .expect("Failed to get memory log channel symbol.");

        let sym_offset = symbol.memory_offset();

        let slots_offset = channel_header.slots_offset + sym_offset;
        let sender = self.mm_log_monitor.upgrade().unwrap().mm_log_sender.clone();
        loop {
            if self.should_stop() {
                break;
            }

            let r = kmem_file
                .read_at(&mut buf, slots_offset)
                .expect("Failed to read kmem file.");
            assert!(r == buf.len());

            let mut logs = Vec::new();

            for chunck in buf.chunks(channel_header.slot_size as usize) {
                let log_item = {
                    let log: Option<ObjectWrapper<AllocatorLog>> =
                        ObjectWrapper::new(&chunck[0..channel_header.element_size as usize]);
                    let log: ObjectWrapper<AllocatorLog> = log.unwrap();

                    if log.is_valid() {
                        Some(log)
                    } else {
                        None
                    }
                };
                if let Some(log_item) = log_item {
                    logs.push(log_item);
                }
            }
            // 收集所有校验和正确的日志
            // info!("valid_cnt: {}, invalid_cnt: {}", valid_cnt, invalid_cnt);
            // info!("to send {} logs", logs.len());
            if !logs.is_empty() {
                sender.send(MMLogWorkerResult::new(logs)).unwrap();
            }
        }
    }

    fn open_kmem_file(&self) -> std::io::Result<std::fs::File> {
        std::fs::OpenOptions::new().read(true).open(&self.kmem_path)
    }

    fn load_header(&self, kmem_file: &mut File) -> ObjectWrapper<MMLogChannel<1>> {
        let mut buf = [0u8; size_of::<MMLogChannel<1>>()];
        let symbol = self
            .mm_log_channel_symbol()
            .expect("Failed to get memory log channel symbol.");

        let sym_offset = symbol.memory_offset();

        let channel_header: Option<ObjectWrapper<MMLogChannel<1>>>;

        loop {
            let _r = kmem_file.read_at(&mut buf, sym_offset);

            let header: ObjectWrapper<MMLogChannel<1>> =
                ObjectWrapper::new(&buf).expect("Failed to parse MMLogChannel header.");
            if header.magic == MMLogChannel::<1>::MM_LOG_CHANNEL_MAGIC {
                info!("channel_header: {:?}", header);
                channel_header = Some(header);
                break;
            } else {
                info!("MM Log Channel not found... Maybe the kernel not started? Or the kernel version is not supported?");
            }

            std::thread::sleep(std::time::Duration::from_secs(1));
        }

        return channel_header.unwrap();
    }

    /// Get the symbol of the memory log channel.
    fn mm_log_channel_symbol(&self) -> Option<Symbol> {
        self.mm_log_monitor
            .upgrade()
            .unwrap()
            .channel_symbol
            .clone()
    }

    /// Check if the monitor worker thread should stop.
    fn should_stop(&self) -> bool {
        self.mm_log_monitor
            .upgrade()
            .map(|mm_log_monitor| {
                mm_log_monitor
                    .stop_child_threads
                    .load(std::sync::atomic::Ordering::Relaxed)
            })
            .unwrap_or(true)
    }
}

/// 内存日志监视器工作线程处理的结果
#[derive(Debug)]
struct MMLogWorkerResult {
    logs: Vec<ObjectWrapper<AllocatorLog>>,
}

impl MMLogWorkerResult {
    /// 创建一个新的内存日志监视器工作线程处理的结果
    ///
    /// ## 参数
    ///
    /// - `logs`：处理的日志
    pub fn new(logs: Vec<ObjectWrapper<AllocatorLog>>) -> Self {
        Self { logs }
    }
}
