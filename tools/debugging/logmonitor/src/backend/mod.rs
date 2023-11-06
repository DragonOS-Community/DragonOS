use std::{
    path::PathBuf,
    sync::{mpsc, Arc, Mutex, RwLock, Weak},
    thread::JoinHandle,
};

use log::info;

use crate::{command::CommandLineArgs, event::Event};

pub mod error;
pub mod event;
mod loader;
mod monitor;

#[derive(Debug)]
pub struct AppBackend {
    _command_line_args: CommandLineArgs,
    _sender_to_frontend: mpsc::Sender<Event>,
    data: Arc<Mutex<BackendData>>,
    main_thread: RwLock<Option<std::thread::JoinHandle<()>>>,
    /// All threads spawned by the backend.(Except the main thread)
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl AppBackend {
    pub fn new(command_line_args: CommandLineArgs, sender: mpsc::Sender<Event>) -> Arc<Self> {
        let r = Arc::new(Self {
            _command_line_args: command_line_args.clone(),
            _sender_to_frontend: sender.clone(),
            data: Arc::new(Mutex::new(BackendData::new())),
            main_thread: RwLock::new(None),
            threads: Mutex::new(Vec::new()),
        });

        r.data.lock().unwrap().kmem_path = Some(PathBuf::from(&command_line_args.kmem));

        let main_thread = {
            let cmdargs = command_line_args.clone();
            let instance = r.clone();
            let sd = sender.clone();
            let dt = r.data.clone();
            std::thread::spawn(move || {
                let mut backend = BackendThread::new(cmdargs, sd, Arc::downgrade(&instance), dt);
                backend.run_main();
            })
        };

        *r.main_thread.write().unwrap() = Some(main_thread);

        return r;
    }
}

#[derive(Debug)]
struct BackendData {
    kernel_metadata: Option<loader::KernelMetadata>,
    /// Path to the QEMU shm which contains the kernel memory.
    kmem_path: Option<PathBuf>,
}

impl BackendData {
    pub fn new() -> Self {
        Self {
            kernel_metadata: None,
            kmem_path: None,
        }
    }
}

#[derive(Debug)]
pub struct BackendThread {
    _sender_to_frontend: mpsc::Sender<Event>,
    command_line_args: CommandLineArgs,
    shared_data: Arc<Mutex<BackendData>>,
    backend_instance: Weak<AppBackend>,
}

impl BackendThread {
    fn new(
        command_line_args: CommandLineArgs,
        sender: mpsc::Sender<Event>,
        backend_instance: Weak<AppBackend>,
        backend_data: Arc<Mutex<BackendData>>,
    ) -> Self {
        Self {
            command_line_args,
            _sender_to_frontend: sender,
            backend_instance,
            shared_data: backend_data,
        }
    }

    pub fn run_main(&mut self) {
        info!("DragonOS Log Monitor started.");
        self.load_kernel();
        self.run_mm_monitor();
        loop {
            // info!("BackendThread::run()");
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }

    /// 启动内存管理监视器
    fn run_mm_monitor(&mut self) {
        info!("run_mm_monitor()");
        let mm_monitor = monitor::mm::MMLogMonitor::new(self.shared_data.clone());
        let handle = std::thread::spawn(move || {
            mm_monitor.run();
        });

        self.backend_instance
            .upgrade()
            .unwrap()
            .threads
            .lock()
            .unwrap()
            .push(handle);
    }

    /// 加载DragonOS内核并初始化
    fn load_kernel(&self) {
        let res = loader::KernelLoader::load(&self.command_line_args.kernel)
            .expect("Failed to load kernel");
        self.shared_data.lock().unwrap().kernel_metadata = Some(res);
    }
}
