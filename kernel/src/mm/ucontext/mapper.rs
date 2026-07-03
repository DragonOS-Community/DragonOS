use super::*;

#[derive(Debug, Hash)]
pub struct UserMapper {
    pub utable: PageMapper,
}

impl UserMapper {
    pub fn new(utable: PageMapper) -> Self {
        return Self { utable };
    }

    /// Copy userspace memory mappings
    /// ## Parameters
    ///
    /// - `umapper`: The userspace mapping to copy from
    /// - `copy_on_write`: Whether to use copy-on-write
    pub unsafe fn clone_from(&mut self, umapper: &mut Self, copy_on_write: bool) {
        self.utable
            .clone_user_mapping(&mut umapper.utable, copy_on_write);
    }
}

impl Drop for UserMapper {
    fn drop(&mut self) {
        if self.utable.is_current() {
            // If the user page table being destroyed belongs to the current process,
            // switch back to the initial kernel page table.
            unsafe { MMArch::set_table(PageTableKind::User, MMArch::initial_page_table()) }
        }
        // Release the page frame occupied by the top-level user page table.
        // Note: before releasing this page frame, the user page table should have been
        // completely freed, otherwise a memory leak will occur.
        unsafe {
            deallocate_page_frames(
                PhysPageFrame::new(self.utable.table().phys()),
                PageFrameCount::new(1),
            )
        };
    }
}
