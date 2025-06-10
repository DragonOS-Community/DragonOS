// pub mod poll_unit;
mod epoll_items;

pub mod shutdown;
pub use epoll_items::EPollItems;

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
