use alloc::{sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::libs::mutex::Mutex;

pub trait SyscoreOps: Send + Sync {
    fn shutdown(&self);
}

lazy_static! {
    static ref SYSCORE_OPS: Mutex<Vec<Arc<dyn SyscoreOps>>> = Mutex::new(Vec::new());
}

#[allow(dead_code)]
pub fn register_syscore_ops(ops: Arc<dyn SyscoreOps>) -> Result<(), SystemError> {
    let mut syscore_ops = SYSCORE_OPS.lock();
    if syscore_ops
        .iter()
        .any(|existing| Arc::ptr_eq(existing, &ops))
    {
        return Err(SystemError::EEXIST);
    }

    syscore_ops.push(ops);
    return Ok(());
}

#[allow(dead_code)]
pub fn unregister_syscore_ops(ops: &Arc<dyn SyscoreOps>) -> Result<(), SystemError> {
    let mut syscore_ops = SYSCORE_OPS.lock();
    let index = syscore_ops
        .iter()
        .position(|existing| Arc::ptr_eq(existing, ops))
        .ok_or(SystemError::ENOENT)?;
    syscore_ops.remove(index);
    return Ok(());
}

pub fn syscore_shutdown() {
    let syscore_ops = {
        let syscore_ops = SYSCORE_OPS.lock();
        syscore_ops.clone()
    };

    for ops in syscore_ops.iter().rev() {
        ops.shutdown();
    }
}
