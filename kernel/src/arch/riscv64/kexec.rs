use crate::init::kexec::Kimage;
use crate::libs::spinlock::SpinLock;
use alloc::rc::Rc;

pub fn machine_kexec_prepare(kimage: Rc<SpinLock<Kimage>>) -> bool {
    false
}

pub fn init_pgtable(kimage: Rc<SpinLock<Kimage>>) {}

pub fn machine_kexec(kimage: Rc<SpinLock<Kimage>>) {}
