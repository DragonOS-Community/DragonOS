//! Isolated owners exercise the production queue and token storage. No live
//! device or DMA descriptor is fabricated by these allocation-failure tests.
use super::*;
use crate::mm::oom::denial;

pub(crate) fn submission_birth() -> Result<bool, SystemError> {
    let queue = BioQueue::try_new()?;
    let tokens = BioTokenMap::try_new(2)?;
    let first = BioRequest::try_new_flush()?;
    let second = BioRequest::try_new_flush()?;
    let first_node = BioQueue::prepare(first.clone())?;
    let second_node = BioQueue::prepare(second.clone())?;
    let first_ctx = BioContext::try_new(first.clone())?;
    let second_ctx = BioContext::try_new(second.clone())?;
    let invalid_ctx = BioContext::try_new(second.clone())?;
    let duplicate_ctx = BioContext::try_new(second.clone())?;
    // Exercise the response allocation independently after request preparation.
    let req = Box::try_new(BlkReq::default()).map_err(|_| SystemError::ENOMEM)?;
    let passed = {
        let _deny = denial::Scope::enter(true);
        let mut passed = denial::probe_denied()
            && matches!(BioQueue::try_new(), Err(SystemError::ENOMEM))
            && matches!(BioQueue::prepare(first.clone()), Err(SystemError::ENOMEM))
            && matches!(BioTokenMap::try_new(2), Err(SystemError::ENOMEM))
            && matches!(BioContext::try_new(first.clone()), Err(SystemError::ENOMEM))
            && Box::try_new(BlkResp::default()).is_err();
        drop(req);
        passed &= queue.submit(first_node).is_ok() && queue.submit(second_node).is_ok();
        passed &= queue.pop().is_some_and(|bio| Arc::ptr_eq(&bio, &first));
        passed &= tokens.insert(0, first_ctx).is_ok() && tokens.insert(1, second_ctx).is_ok();
        // Checked insertion must return the unpublished owner on both failures.
        passed &= tokens.insert(2, invalid_ctx).is_err();
        passed &= tokens.insert(0, duplicate_ctx).is_err();
        passed &= !tokens.is_empty();
        passed &= tokens
            .pop()
            .is_some_and(|ctx| Arc::ptr_eq(&ctx.bio, &first));
        let mut pending = queue.stop_and_drain();
        passed &= pending
            .pop_node()
            .is_some_and(|node| Arc::ptr_eq(&node.into_value(), &second));
        passed &= pending.is_empty() && queue.pop().is_none();
        passed &= tokens
            .pop()
            .is_some_and(|ctx| Arc::ptr_eq(&ctx.bio, &second))
            && tokens.is_empty()
            && denial::probe_denied();
        passed
    };
    // A rejected preparation leaves the same objects reusable after pressure.
    let node = BioQueue::prepare(first.clone())?;
    let rejected = queue.submit(node).is_err();
    let context = BioContext::try_new(first)?;
    let retry = tokens.insert(1, context).is_ok() && tokens.pop().is_some() && tokens.is_empty();
    Ok(passed && rejected && retry)
}
