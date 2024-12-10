use crate::{ebpf::STACK_SIZE, vec, Vec};

pub struct StackFrame {
    return_address: u64,
    saved_registers: [u64; 4],
    sp: u64,
    frame: Vec<u8>,
}

impl StackFrame {
    /// Create a new stack frame
    ///
    /// The stack frame is created with a capacity of `STACK_SIZE` == 512 bytes
    pub fn new() -> Self {
        Self {
            sp: 0,
            return_address: 0,
            saved_registers: [0; 4],
            frame: vec![0; STACK_SIZE],
        }
    }

    /// Create a new stack frame with a given capacity
    #[allow(unused)]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            sp: 0,
            return_address: 0,
            saved_registers: [0; 4],
            frame: vec![0; capacity],
        }
    }

    /// The capacity of the stack frame
    pub fn len(&self) -> usize {
        self.frame.len()
    }

    pub fn as_ptr(&self) -> *const u8 {
        self.frame.as_ptr()
    }

    pub fn as_slice(&self) -> &[u8] {
        self.frame.as_slice()
    }
    /// Save the callee-saved registers
    pub fn save_registers(&mut self, regs: &[u64]) {
        self.saved_registers.copy_from_slice(regs);
    }

    /// Get the callee-saved registers
    pub fn get_registers(&self) -> [u64; 4] {
        self.saved_registers
    }

    /// Save the return address
    pub fn save_return_address(&mut self, address: u64) {
        self.return_address = address;
    }

    /// Get the return address
    pub fn get_return_address(&self) -> u64 {
        self.return_address
    }

    /// Save the stack pointer
    pub fn save_sp(&mut self, sp: u64) {
        self.sp = sp;
    }

    /// Get the stack pointer
    pub fn get_sp(&self) -> u64 {
        self.sp
    }
}
