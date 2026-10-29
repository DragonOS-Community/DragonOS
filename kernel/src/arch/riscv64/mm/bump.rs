use crate::mm::{allocator::bump::BumpAllocator, MemoryManagementArch, PhysMemoryArea};

impl<MMA: MemoryManagementArch> BumpAllocator<MMA> {
    pub unsafe fn arch_remain_areas(
        _ret_areas: &mut [PhysMemoryArea],
        res_count: usize,
    ) -> Option<usize> {
        // todo: riscv64
        return (res_count <= _ret_areas.len()).then_some(res_count);
    }
}
