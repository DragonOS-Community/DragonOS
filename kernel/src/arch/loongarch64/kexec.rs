use crate::init::kexec::Kimage;
use crate::libs::spinlock::SpinLock;
use alloc::rc::Rc;
use system_error::SystemError;

pub fn machine_kexec_prepare(kimage: Rc<SpinLock<Kimage>>) -> Result<(), SystemError> {
    Ok(())
}

pub fn init_pgtable(kimage: Rc<SpinLock<Kimage>>) -> Result<(), SystemError> {
    Ok(())
}

pub fn machine_kexec(kimage: Rc<SpinLock<Kimage>>) -> Result<(), SystemError> {
    Ok(())
}
