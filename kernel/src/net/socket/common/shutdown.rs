
bitflags! {
    /// @brief 用于指定socket的关闭类型
    /// 参考：https://code.dragonos.org.cn/xref/linux-6.1.9/include/net/sock.h?fi=SHUTDOWN_MASK#1573
    pub struct ShutdownType: u8 {
        const SHUT_RD = 0;
        const SHUT_WR = 1;
        const SHUT_RDWR = 2;
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Shutdown {
    shutdown_type: ShutdownType,
}

impl Shutdown {
    pub fn new() -> Self {
        Self {
            shutdown_type: ShutdownType::empty(),
        }
    }

    pub fn set_shutdown_type(&mut self, shutdown_type: ShutdownType) {
        self.shutdown_type = shutdown_type;
    }

    pub fn get_shutdown_type(&self) -> ShutdownType {
        self.shutdown_type
    }

    pub fn reset_shutdown_type(&mut self) {
        self.shutdown_type = ShutdownType::empty();
    }

    pub fn is_recv_shutdown(&self) -> bool {
        self.shutdown_type.contains(ShutdownType::RECV_SHUTDOWN)
    }

    pub fn is_send_shutdown(&self) -> bool {
        self.shutdown_type.contains(ShutdownType::SEND_SHUTDOWN)
    }
}