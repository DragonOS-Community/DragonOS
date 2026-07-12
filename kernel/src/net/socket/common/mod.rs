mod epoll_items;
mod getsockopt;
mod shutdown;
mod timeval;

pub use epoll_items::EPollItems;
pub use getsockopt::{
    write_i32_getsockopt, write_i32_getsockopt_ipv4, write_linger_getsockopt, write_u32_getsockopt,
};
pub use shutdown::ShutdownBit;
pub use timeval::{
    parse_timeval_opt, parse_timeval_ticks, write_timeval_opt, write_timeval_ticks,
    INFINITE_TIMEOUT_TICKS,
};

// /// @brief 在trait Socket的metadata函数中返回该结构体供外部使用
// #[derive(Debug, Clone)]
// pub struct Metadata {
//     /// 接收缓冲区的大小
//     pub rx_buf_size: usize,
//     /// 发送缓冲区的大小
//     pub tx_buf_size: usize,
//     /// 元数据的缓冲区的大小
//     pub metadata_buf_size: usize,
//     /// socket的选项
//     pub options: SocketOptions,
// }
