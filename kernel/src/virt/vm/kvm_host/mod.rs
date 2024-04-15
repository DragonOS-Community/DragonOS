use core::sync::atomic::AtomicUsize;

use alloc::string::String;

use crate::mm::ucontext::AddressSpace;

pub mod vcpu;

const KVM_ADDRESS_SPACE_NUM: usize = 1;

pub struct KvmMemSlots {
    /// 最后一次使用到的内存插槽
    last_use: AtomicUsize,
    /// 存储虚拟地址（hva）和内存插槽之间的映射关系
    // Rbt
    /// 用于存储全局页帧号（gfn）和内存插槽之间的映射关系
    // Rbt
    /// 将内存插槽的ID映射到对应的内存插槽。
    // HashMap
    /// 节点索引
    node_idx: usize,
}

pub struct Vm {
    mm: AddressSpace,
    max_vcpus: usize,
    name: String,
}
