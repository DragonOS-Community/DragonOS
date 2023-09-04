use thingbuf::mpsc;

pub struct StdoutDevice {
    stdin_rx: mpsc::Receiver<u8>,
    stdin_tx: mpsc::Sender<u8>,
}
