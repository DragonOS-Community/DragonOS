use std::sync::mpsc;

use log::LevelFilter;
use simple_logger::LogBackend;

use crate::command::CommandLineArgs;

/// Initialize the logging system.
pub fn init(cmd_args: &CommandLineArgs) -> LoggingInitResult {
    let mut builder = simple_logger::SimpleLogger::new().with_level(LevelFilter::Info);

    let mut result = LoggingInitResult::new(None);

    if cmd_args.tui {
        let channel: (mpsc::Sender<String>, mpsc::Receiver<String>) = mpsc::channel::<String>();
        builder = builder.with_backend(Box::new(TUILoggingBackend::new(channel.0)));
        result.tui_receiver = Some(channel.1);
    }

    builder.init().expect("failed to initialize logging");

    return result;
}

#[derive(Debug)]
pub struct LoggingInitResult {
    /// Logging backend receiver.
    pub tui_receiver: Option<mpsc::Receiver<String>>,
}

impl LoggingInitResult {
    pub fn new(tui_receiver: Option<mpsc::Receiver<String>>) -> Self {
        Self { tui_receiver }
    }
}

pub struct TUILoggingBackend {
    sender: mpsc::Sender<String>,
}

impl TUILoggingBackend {
    pub fn new(sender: mpsc::Sender<String>) -> Self {
        Self { sender }
    }
}

impl LogBackend for TUILoggingBackend {
    fn log(&self, message: String) {
        self.sender.send(message).ok();
    }
}
