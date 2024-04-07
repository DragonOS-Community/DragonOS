use alloc::vec::Vec;

use self::cache_iter::FailData;

mod cache_block;
mod cache_iter;
pub mod cached_block_device;
pub const BLOCK_SIZE_LOG: usize = 9;
///块大小这里固定为512
pub const BLOCK_SIZE: usize = 1 << BLOCK_SIZE_LOG;
///这里规定Cache的threshold大小，单位为：MB
pub const CACHE_THRESHOLD: usize = 64;

pub enum BlockCacheError {
    BlockSizeError,
    InsufficientCacheSpace,
    StaticParameterError,
    BlockFaultError(Vec<FailData>),
}
