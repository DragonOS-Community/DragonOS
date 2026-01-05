#![no_std]

/// Wait for a condition to become true.
///
/// This macro will wait for a condition to become true.
///
/// ## Parameters
///
/// - `$wq`: The wait queue to wait on.
/// - `$condition`: The condition to wait for. (you can pass a function or a boolean expression)
/// - `$cmd`: The command to execute while waiting.
#[macro_export]
macro_rules! wq_wait_event_interruptible {
    ($wq:expr, $condition: expr, $cmd: expr) => {{
        let mut retval = Ok(());
        if !$condition {
            retval = wait_queue_macros::_wq_wait_event_interruptible!($wq, $condition, $cmd);
        }

        retval
    }};
}

#[macro_export]
#[allow(clippy::crate_in_macro_def)]
macro_rules! _wq_wait_event_interruptible {
    ($wq:expr, $condition: expr, $cmd: expr) => {{
        wait_queue_macros::__wq_wait_event!($wq, $condition, true, Ok(()), {
            $cmd;
        })
    }};
}

#[macro_export]
macro_rules! __wq_wait_event(
    ($wq:expr, $condition: expr, $interruptible: expr, $ret: expr, $cmd:expr) => {{
        let mut retval = $ret;
        loop {
            if $condition { break; }
            let res = if $interruptible {
                $wq.wait_event_interruptible(|| $condition, Some(|| $cmd))
            } else {
                $wq.wait_event_uninterruptible(|| $condition, Some(|| $cmd))
            };
            if let Err(e) = res {
                retval = Err(e);
                break;
            }
            if $condition { break; }
        }

        retval
    }};
);
