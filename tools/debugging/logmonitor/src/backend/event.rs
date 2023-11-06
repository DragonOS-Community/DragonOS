#[derive(Debug, Clone)]
pub enum BackendEvent {
    StartUp(StartUpEvent),
}

#[derive(Debug, Clone)]
pub struct StartUpEvent {
    started: bool,
    message: String,
    timestamp: std::time::Instant,
}
