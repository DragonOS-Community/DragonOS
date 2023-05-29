/// PCB中与架构相关的信息
#[derive(Debug)]
pub struct ArchPCBInfo {
    rflags: usize,
    rbx: usize,
    r12: usize,
    r13: usize,
    r14: usize,
    r15: usize,
    rbp: usize,
    rsp: usize,
    rip: usize,
    cr2: usize,
    fsbase: usize,
    gsbase: usize,
}

impl ArchPCBInfo {
    pub fn new() -> Self {
        Self {
            rflags: 0,
            rbx: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rbp: 0,
            rsp: 0,
            rip: 0,
            cr2: 0,
            fsbase: 0,
            gsbase: 0,
        }
    }

    pub fn set_stack(&mut self, stack: usize) {
        self.rsp = stack;
    }
    
    pub unsafe fn push_to_stack(&mut self, value: usize) {
        self.rsp -= core::mem::size_of::<usize>();
        *(self.rsp as *mut usize) = value;
    }

    pub unsafe fn pop_from_stack(&mut self) -> usize {
        let value = *(self.rsp as *const usize);
        self.rsp += core::mem::size_of::<usize>();
        value
    }
}
