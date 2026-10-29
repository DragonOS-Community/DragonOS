//! Bounded seq-style member enumeration, separate from scalar text snapshots.

use alloc::{sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::{
    cgroup::CgroupNode,
    process::{
        namespace::pid_namespace::INIT_PID_NAMESPACE,
        pid::{Pid, PidType},
        ProcessControlBlock,
    },
};

use super::files::{self, CgroupCoreFile};

const WINDOW: usize = 4096;
const BATCH: usize = 32;
// A decimal usize plus newline, including nonstandard future PID widths.
const PID_LINE: usize = 21;

#[derive(Debug, Default)]
pub(super) struct MembershipRead {
    bytes: Vec<u8>,
    from: usize,
    next_offset: usize,
    next_pid: usize,
    bound: usize,
    initialized: bool,
}

impl MembershipRead {
    pub(super) fn read(
        &mut self,
        cgroup: &Arc<CgroupNode>,
        ty: CgroupCoreFile,
        generation: u64,
        offset: usize,
        len: usize,
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        let want = len.min(buf.len());
        if want == 0 {
            return Ok(0);
        }
        if !self.initialized || offset == 0 || offset != self.next_offset {
            self.bytes.clear();
            self.from = 0;
            self.next_offset = 0;
            self.next_pid = 1;
            self.bound = INIT_PID_NAMESPACE.published_pid_bound();
            self.initialized = true;
        }
        // A positional read replays records, rather than caching all prior text.
        while self.next_offset < offset {
            if self.from == self.bytes.len() {
                self.refill(cgroup, ty, generation)?;
                if self.bytes.is_empty() {
                    self.next_offset = offset;
                    return Ok(0);
                }
            }
            let skip = (offset - self.next_offset).min(self.bytes.len() - self.from);
            self.from += skip;
            self.next_offset += skip;
        }
        if self.from == self.bytes.len() {
            self.refill(cgroup, ty, generation)?;
        }
        let count = want.min(self.bytes.len() - self.from);
        buf[..count].copy_from_slice(&self.bytes[self.from..self.from + count]);
        self.from += count;
        self.next_offset = offset + count;
        Ok(count)
    }

    fn refill(
        &mut self,
        cgroup: &Arc<CgroupNode>,
        ty: CgroupCoreFile,
        generation: u64,
    ) -> Result<(), SystemError> {
        self.bytes.clear();
        self.from = 0;
        let result = self.fill_window(cgroup, ty, generation);
        if result.is_err() {
            // A failed refill must not publish an incomplete older epoch on
            // a later read at the same nonzero byte offset.
            self.bytes.clear();
            self.initialized = false;
        }
        result
    }

    fn fill_window(
        &mut self,
        cgroup: &Arc<CgroupNode>,
        ty: CgroupCoreFile,
        generation: u64,
    ) -> Result<(), SystemError> {
        files::check_read_source(cgroup, ty, 0, generation)?;
        if self.bytes.capacity() < WINDOW {
            self.bytes
                .try_reserve_exact(WINDOW)
                .map_err(|_| SystemError::ENOMEM)?;
        }
        let processes = matches!(ty, CgroupCoreFile::Procs);
        {
            let _update = crate::cgroup::lock();
            if processes && cgroup.is_threaded() {
                return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
            }
        }
        while self.next_pid <= self.bound && WINDOW - self.bytes.len() >= PID_LINE {
            let mut batch: [Option<Arc<Pid>>; BATCH] = core::array::from_fn(|_| None);
            let slots = BATCH.min((WINDOW - self.bytes.len()) / PID_LINE);
            let count = INIT_PID_NAMESPACE.read_pid_batch(
                &mut self.next_pid,
                self.bound,
                &mut batch[..slots],
            );
            for identity in batch.iter_mut().take(count).filter_map(Option::take) {
                // No Arc upgrade or last-owner retirement under PID task locks.
                let task = identity
                    .first_task_weak(PidType::PID)
                    .and_then(|task| task.upgrade());
                let Some(task) = task else { continue };
                let representative = if processes {
                    live_representative(&task)
                } else {
                    Some(task.clone())
                };
                let Some(representative) = representative else {
                    continue;
                };
                // These identity owners also retire after both cgroup locks.
                let mut display = None;
                let mut representative_tgid = None;
                let emit = {
                    let _update = crate::cgroup::lock();
                    files::check_live_file(cgroup, ty, generation)?;
                    if processes && cgroup.is_threaded() {
                        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                    }
                    let _accounting = crate::cgroup::cgroup_accounting_lock().lock();
                    if representative.is_exited() || (processes && !task.is_thread_group_leader()) {
                        false
                    } else {
                        let node = representative.task_cgroup_node();
                        let matches = Arc::ptr_eq(&node, cgroup)
                            || (processes
                                && node.is_threaded()
                                && Arc::ptr_eq(&node.resource_domain(), cgroup));
                        if matches {
                            display = task.task_pid_ptr(if processes {
                                PidType::TGID
                            } else {
                                PidType::PID
                            });
                            representative_tgid = if processes {
                                representative.task_pid_ptr(PidType::TGID)
                            } else {
                                None
                            };
                            !processes
                                || display
                                    .as_ref()
                                    .zip(representative_tgid.as_ref())
                                    .is_some_and(|(leader, member)| Arc::ptr_eq(leader, member))
                        } else {
                            false
                        }
                    }
                };
                if emit {
                    if let Some(pid) = &display {
                        append_pid(&mut self.bytes, pid.pid_vnr().data());
                    }
                }
            }
        }
        Ok(())
    }
}

/// A dead leader can retain a live process, whose threads may have migrated
/// since the leader exited. Never use that leader's stale membership node.
fn live_representative(task: &Arc<ProcessControlBlock>) -> Option<Arc<ProcessControlBlock>> {
    if !task.is_thread_group_leader() {
        return None;
    }
    if !task.is_exited() {
        return Some(task.clone());
    }
    let mut index = 0;
    loop {
        // Copy one observer; upgrade/drop outside ThreadInfo and cgroup locks.
        let weak = task
            .threads_read_irqsave()
            .group_tasks()
            .get(index)
            .cloned()?;
        index += 1;
        if let Some(member) = weak.upgrade() {
            if !member.is_exited() {
                return Some(member);
            }
        }
    }
}

fn append_pid(bytes: &mut Vec<u8>, mut pid: usize) {
    let mut line = [0u8; PID_LINE];
    let mut from = line.len() - 1;
    line[from] = b'\n';
    loop {
        from -= 1;
        line[from] = b'0' + (pid % 10) as u8;
        pid /= 10;
        if pid == 0 {
            break;
        }
    }
    bytes.extend_from_slice(&line[from..]);
}
