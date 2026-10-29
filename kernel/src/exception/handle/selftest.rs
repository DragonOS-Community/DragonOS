//! Dispatch the real action list under heap denial, with private descriptors.
use super::*;
use crate::{exception::irqdesc::IrqAction, mm::oom::denial};
use alloc::string::ToString;
use core::sync::atomic::{AtomicUsize, Ordering};

static SEQUENCE: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
struct Handler(usize);

impl super::super::irqdesc::IrqHandler for Handler {
    fn handle(
        &self,
        _irq: IrqNumber,
        _static_data: Option<&dyn IrqHandlerData>,
        _dynamic_data: Option<Arc<dyn IrqHandlerData>>,
    ) -> Result<IrqReturn, SystemError> {
        SEQUENCE
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
                Some(old * 10 + self.0)
            })
            .unwrap();
        if self.0 == 3 {
            Err(SystemError::EIO)
        } else {
            Ok(IrqReturn::Handled)
        }
    }
}

static FIRST: Handler = Handler(1);
static SECOND: Handler = Handler(2);
static THIRD: Handler = Handler(3);

fn dispatch_denied(desc: &Arc<IrqDesc>, expected: usize) -> bool {
    SEQUENCE.store(0, Ordering::Relaxed);
    let _scope = denial::Scope::enter(true);
    let before = denial::probe_denied();
    let result = do_handle_irq_event(desc);
    before
        && result == Err(SystemError::EIO)
        && SEQUENCE.load(Ordering::Relaxed) == expected
        && denial::probe_denied()
}

pub(super) fn run() -> Result<bool, SystemError> {
    // No hardware IRQ is registered or masked by this fixture. All births and
    // mutations precede denial; page-retirement's TEST_LOCK serializes callers.
    let irq = IrqNumber::new(0);
    let desc = IrqDesc::new(irq, None, IrqStatus::empty());
    let first = IrqAction::new(irq, "first".to_string(), Some(&FIRST), None);
    let second = IrqAction::new(irq, "second".to_string(), Some(&SECOND), None);
    let third = IrqAction::new(irq, "third".to_string(), Some(&THIRD), None);
    {
        let mut inner = desc.inner();
        inner.add_action(first.clone());
        inner.add_action(second.clone());
        inner.add_action(third.clone());
    }
    let old = desc.actions();
    let mut ok = dispatch_denied(&desc, 123);
    let removed = desc.inner().remove_action_at(1);
    ok &= Arc::ptr_eq(&removed, &second)
        && old.len() == 3
        && Arc::ptr_eq(&old[1], &second)
        && desc.inner().actions().len() == 2;
    ok &= dispatch_denied(&desc, 13);
    let retained = desc.actions();
    desc.inner().remove_action(&first);
    ok &= retained.len() == 2 && desc.inner().actions().len() == 1;
    ok &= dispatch_denied(&desc, 3);
    desc.inner().clear_actions();
    ok &= old.len() == 3 && retained.len() == 2;
    {
        let _scope = denial::Scope::enter(true);
        ok &= denial::probe_denied() && do_handle_irq_event(&desc) == Ok(());
    }
    Ok(ok)
}
