use system_error::SystemError;

pub trait Vcpu: Send + Sync {
    /// Virtualize the CPU
    fn virtualize_cpu(&mut self) -> Result<(), SystemError>;

    #[allow(dead_code)]
    fn devirtualize_cpu(&self) -> Result<(), SystemError>;

    /// Gets the index of the current logical/virtual processor
    #[allow(dead_code)]
    fn id(&self) -> u32;
}
