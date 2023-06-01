/// @Auther: Kong
/// @Date: 2023-05-08 12:49:02
/// @FilePath: /DragonOS/kernel/src/mm/allocator/mod.rs
/// @Description: 
/// 
pub mod buddy;
pub mod bump;
pub mod c;
pub mod page_frame;
pub mod slab;
// pub mod dummy;
// pub mod buddy2;
// pub mod my_list;

// #[global_allocator]
// static ALLOCATOR: dummy::Dummy = dummy::Dummy;
// static ALLOCATOR: LockedHeap = LockedHeap::empty();