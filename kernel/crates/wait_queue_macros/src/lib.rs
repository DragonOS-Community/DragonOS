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
            crate::sched::schedule(SchedMode::SM_NONE)
        })
    }};
}

#[macro_export]
macro_rules! __wq_wait_event(
    ($wq:expr, $condition: expr, $interruptible: expr, $ret: expr, $cmd:expr) => {{
        let mut retval = $ret;
        let mut exec_finish_wait = true;
        loop {
            let x = $wq.prepare_to_wait_event($interruptible);
            if $condition {
                break;
            }

            if $interruptible && !x.is_ok() {
                retval = x;
                exec_finish_wait = false;
                break;
            }

            $cmd;
        }
        if exec_finish_wait {
            $wq.finish_wait();
        }

        retval
    }};
);
