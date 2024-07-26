use system_error::SystemError;

#[allow(dead_code)]
pub trait Vcpu: Send + Sync {
    /// Virtualize the CPU
    fn virtualize_cpu(&mut self) -> Result<(), SystemError>;
    fn devirtualize_cpu(&self) -> Result<(), SystemError>;
    /// Gets the index of the current logical/virtual processor
    fn id(&self) -> u32;
}
