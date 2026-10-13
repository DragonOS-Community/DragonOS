use super::*;

#[derive(Debug, Hash)]
pub struct UserMapper {
    pub utable: PageMapper,
}

impl UserMapper {
    pub fn new(utable: PageMapper) -> Self {
        return Self { utable };
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
        if unsafe { self.utable.release_user_tables() } {
            return;
        }
        unsafe {
            deallocate_page_frames(
                PhysPageFrame::new(self.utable.table().phys()),
                PageFrameCount::new(1),
            )
        };
    }
}
