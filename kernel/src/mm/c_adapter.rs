//! 这是暴露给C的接口，用于在C语言中使用Rust的内存分配器。

use core::intrinsics::unlikely;

use alloc::vec::Vec;
use hashbrown::HashMap;
use log::error;
use system_error::SystemError;

use crate::libs::spinlock::SpinLock;

use super::{mmio_buddy::mmio_pool, VirtAddr};

lazy_static! {
    // 用于记录内核分配给C的空间信息
    static ref C_ALLOCATION_MAP: SpinLock<HashMap<VirtAddr, (VirtAddr, usize, usize)>> = SpinLock::new(HashMap::new());
}

#[no_mangle]
pub unsafe extern "C" fn kzalloc(size: usize, _gfp: u64) -> usize {
    // debug!("kzalloc: size: {size}");
    return do_kmalloc(size, true);
}

#[no_mangle]
pub unsafe extern "C" fn kmalloc(size: usize, _gfp: u64) -> usize {
    // debug!("kmalloc: size: {size}");
    // 由于C代码不规范，因此都全部清空
    return do_kmalloc(size, true);
}

fn do_kmalloc(size: usize, _zero: bool) -> usize {
    let space: Vec<u8> = vec![0u8; size];

    assert!(space.len() == size);
    let (ptr, len, cap) = space.into_raw_parts();
    if !ptr.is_null() {
        let vaddr = VirtAddr::new(ptr as usize);
        let mut guard = C_ALLOCATION_MAP.lock();
        if unlikely(guard.contains_key(&vaddr)) {
            drop(guard);
            unsafe {
                drop(Vec::from_raw_parts(vaddr.data() as *mut u8, len, cap));
            }
            panic!(
                "do_kmalloc: vaddr {:?} already exists in C Allocation Map, query size: {size}, zero: {_zero}",
                vaddr
            );
        }
        // 插入到C Allocation Map中
        guard.insert(vaddr, (vaddr, len, cap));
        return vaddr.data();
    } else {
        return SystemError::ENOMEM.to_posix_errno() as i64 as usize;
    }
}

#[no_mangle]
pub unsafe extern "C" fn kfree(vaddr: usize) -> usize {
    let vaddr = VirtAddr::new(vaddr);
    let mut guard = C_ALLOCATION_MAP.lock();
    let p = guard.remove(&vaddr);
    drop(guard);

    if p.is_none() {
        error!("kfree: vaddr {:?} not found in C Allocation Map", vaddr);
        return SystemError::EINVAL.to_posix_errno() as i64 as usize;
    }
    let (vaddr, len, cap) = p.unwrap();
    drop(Vec::from_raw_parts(vaddr.data() as *mut u8, len, cap));
    return 0;
}

/// @brief 创建一块mmio区域，并将vma绑定到initial_mm
///
/// @param size mmio区域的大小（字节）
///
/// @param vm_flags 要把vma设置成的标志
///
/// @param res_vaddr 返回值-分配得到的虚拟地址
///
/// @param res_length 返回值-分配的虚拟地址空间长度
///
/// @return int 错误码
#[no_mangle]
unsafe extern "C" fn rs_mmio_create(
    size: u32,
    _vm_flags: u64,
    res_vaddr: *mut u64,
    res_length: *mut u64,
) -> i32 {
    // debug!("mmio_create");
    let r = mmio_pool().create_mmio(size as usize);
    if let Err(e) = r {
        return e.to_posix_errno();
    }
    let space_guard = r.unwrap();
    *res_vaddr = space_guard.vaddr().data() as u64;
    *res_length = space_guard.size() as u64;
    // 由于space_guard drop的时候会自动释放内存，所以这里要忽略它的释放
    core::mem::forget(space_guard);
    return 0;
}

/// @brief 取消mmio的映射并将地址空间归还到buddy中
///
/// @param vaddr 起始的虚拟地址
///
/// @param length 要归还的地址空间的长度
///
/// @return Ok(i32) 成功返回0
///
/// @return Err(i32) 失败返回错误码
#[no_mangle]
pub unsafe extern "C" fn rs_mmio_release(vaddr: u64, length: u64) -> i32 {
    return mmio_pool()
        .release_mmio(VirtAddr::new(vaddr as usize), length as usize)
        .unwrap_or_else(|err| err.to_posix_errno());
}
