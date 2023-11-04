use std::sync::{mpsc, Arc, Mutex};

use crate::{command::CommandLineArgs, event::Event};

#[derive(Debug)]
pub struct AppBackend {
    command_line_args: CommandLineArgs,
    sender_to_frontend: mpsc::Sender<Event>,
    data: Arc<Mutex<BackendData>>,
    main_thread: Option<std::thread::JoinHandle<()>>,
}

impl AppBackend {
    pub fn new(command_line_args: CommandLineArgs, sender: mpsc::Sender<Event>) -> Self {
        let main_thread = {
            let cmdargs = command_line_args.clone();
            let sd = sender.clone();
            std::thread::spawn(move || {
                let mut backend = BackendThread::new(cmdargs, sd);
                backend.run();
            })
        };

        Self {
            command_line_args,
            sender_to_frontend: sender,
            data: Arc::new(Mutex::new(BackendData::new())),
            main_thread: Some(main_thread),
        }
    }
}

#[derive(Debug)]
struct BackendData {
    // todo:
}

impl BackendData {
    pub fn new() -> Self {
        Self {}
    }
}

pub struct BackendThread {
    data: Arc<Mutex<BackendData>>,
    sender_to_frontend: mpsc::Sender<Event>,
    command_line_args: CommandLineArgs,
}

impl BackendThread {
    pub fn new(command_line_args: CommandLineArgs, sender: mpsc::Sender<Event>) -> Self {
        Self {
            command_line_args,
            sender_to_frontend: sender,
            data: Arc::new(Mutex::new(BackendData::new())),
        }
    }

    pub fn run(&mut self) {
        loop {
            println!("BackendThread::run()");
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
}
