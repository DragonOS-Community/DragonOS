pub mod i_cache;

pub use i_cache::{
    lookup_inode_by_pid, 
    cache_pid_inode, 
    uncache_pid_inode, 
    allocate_pid_inode_id,
};