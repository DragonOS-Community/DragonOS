use system_error::SystemError;
use core::intrinsics::unlikely;

pub trait Protocol{
    fn close(&self);
    // fn first_false_index(&self, proto_inuse_idx:usize, proto_inuse_nr:usize)->usize;
}
pub struct Proto<'a> {
    name: &'a str,
    // owner: THIS_MODULE,
    obj_size: usize,
    // inuse_idx: usize,
}
impl Protocol for Proto<'_>{
    fn close(&self) {
        // Implementation of the function
    }
    // fn first_false_index(&self, proto_inuse_idx:usize, proto_inuse_nr:usize)->usize{
    //     let mut i = 0;
    //     while i < proto_inuse_nr {
    //         if !test_bit(i, proto_inuse_idx) {
    //             return i;
    //         }
    //         i += 1;
    //     }
    //     return i;
    // }
}
pub static NETLINK_PROTO: Proto = Proto {
    name: "NETLINK",
    // owner: THIS_MODULE,
    obj_size: core::mem::size_of::<Proto>(),
};

pub fn proto_register(proto:&Proto,alloc_slab:i32)->i32{
    // Implementation of the function
    let ret = SystemError::ENOBUFS;

    ret as i32
}


// pub fn assign_proto_idx(prot:&Proto)->i32{
//     prot.inuse_idx = prot.first_false_index(proto_inuse_idx, PROTO_INUSE_NR);

//     if unlikely(prot.inuse_idx == PROTO_INUSE_NR - 1) {
//         pr_err("PROTO_INUSE_NR exhausted\n");
//         return -ENOSPC;
//     }

//     prot.inuse_idx.set_bit(proto_inuse_idx);
//     return 0;
// }