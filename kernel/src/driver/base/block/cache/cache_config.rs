pub const BLOCK_SIZE_LOG:usize=9;
///块大小这里固定为512
pub const BLOCK_SIZE: usize = 1<<BLOCK_SIZE_LOG;
///这里规定Cache的threshold大小，单位为：MB
pub const CACHE_THRESHOLD:usize=64;

