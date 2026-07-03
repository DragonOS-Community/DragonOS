use super::*;

#[derive(Debug)]
pub struct UserStack {
    // Stack bottom address
    pub(super) stack_bottom: VirtAddr,
    // Currently mapped size
    pub(super) mapped_size: usize,
    /// Stack top address (this value must be determined carefully! It may not stay in sync
    /// with the user stack's real stack top in real time! Be careful!)
    pub(super) current_sp: VirtAddr,
    /// User-defined stack size limit
    pub(super) max_limit: usize,
}

impl UserStack {
    /// Default user stack bottom address
    pub const DEFAULT_USER_STACK_BOTTOM: VirtAddr = MMArch::USER_STACK_START;
    /// Default user stack size is 8MB
    pub const DEFAULT_USER_STACK_SIZE: usize = 8 * 1024 * 1024;
    /// Number of guard pages for the user stack
    pub const GUARD_PAGES_NUM: usize = 4;

    /// Create a user stack
    pub fn new(
        vm: &mut InnerAddressSpace,
        stack_bottom: Option<VirtAddr>,
        stack_size: usize,
    ) -> Result<Self, SystemError> {
        let stack_bottom = stack_bottom.unwrap_or(Self::DEFAULT_USER_STACK_BOTTOM);
        assert!(stack_bottom.check_aligned(MMArch::PAGE_SIZE));

        // Layout
        // -------------- high->sp
        // | stack pages|
        // |------------|
        // | not mapped |
        // -------------- low

        let prot_flags = ProtFlags::PROT_READ | ProtFlags::PROT_WRITE | ProtFlags::PROT_EXEC;
        let map_flags = MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS | MapFlags::MAP_GROWSDOWN;

        let stack_size = page_align_up(stack_size);

        // log::info!(
        //     "UserStack stack_range: {:#x} - {:#x}",
        //     stack_bottom.data() - stack_size,
        //     stack_bottom.data()
        // );

        vm.map_anonymous(
            stack_bottom - stack_size,
            stack_size,
            prot_flags,
            map_flags,
            false,
            false,
        )?;

        let max_limit = core::cmp::max(Self::DEFAULT_USER_STACK_SIZE, stack_size);

        let user_stack = UserStack {
            stack_bottom,
            mapped_size: stack_size,
            current_sp: stack_bottom,
            max_limit,
        };

        return Ok(user_stack);
    }

    /// Get the stack top address
    ///
    /// Note that this value may not update in real time if the user stack's top address changes!
    pub fn sp(&self) -> VirtAddr {
        return self.current_sp;
    }

    pub unsafe fn set_sp(&mut self, sp: VirtAddr) {
        self.current_sp = sp;
    }

    /// Only clones the user stack metadata, without cloning the user stack's contents/mappings
    pub unsafe fn clone_info_only(&self) -> Self {
        return Self {
            stack_bottom: self.stack_bottom,
            mapped_size: self.mapped_size,
            current_sp: self.current_sp,
            max_limit: self.max_limit,
        };
    }

    /// Get the current user stack size (excluding guard pages)
    pub fn stack_size(&self) -> usize {
        return self.mapped_size;
    }

    /// Set the maximum size of the current user stack
    pub fn set_max_limit(&mut self, max_limit: usize) {
        self.max_limit = max_limit;
    }

    /// Get the maximum size limit of the current user stack
    pub fn max_limit(&self) -> usize {
        self.max_limit
    }
}
