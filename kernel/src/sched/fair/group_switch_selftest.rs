//! Private two-level witness. Calls the production pick path; never schedules.
use super::*;
use crate::{arch::CurrentIrqArch, exception::InterruptArch, smp::cpu::ProcessorId};
use system_error::SystemError;

#[derive(Default)]
pub(crate) struct Report {
    pub(crate) setup: bool,
    pub(crate) leaf_switch: bool,
    pub(crate) token_preserved: bool,
    pub(crate) b_eligible: bool,
    pub(crate) edf_b: bool,
    pub(crate) winner_b: bool,
    pub(crate) cleanup: bool,
    pub(crate) old_token: i64,
    pub(crate) observed_token: i64,
    pub(crate) deadline: u64,
}

impl Report {
    pub(crate) fn ok(&self) -> bool {
        self.setup
            && self.leaf_switch
            && self.token_preserved
            && self.b_eligible
            && self.edf_b
            && self.winner_b
            && self.cleanup
    }
}

pub(crate) fn run() -> Result<Report, SystemError> {
    if !CurrentIrqArch::is_irq_enabled() {
        return Err(SystemError::EINVAL);
    }
    // All fallible preparation and final resource destruction run IRQ-on.
    // This existing helper never attaches PID links or publishes the task.
    let first = crate::process::pid::unpublished_task_for_selftest()?;
    let second = crate::process::pid::unpublished_task_for_selftest()?;
    let rq =
        Arc::try_new(CpuRunQueue::new(ProcessorId::new(0))).map_err(|_| SystemError::ENOMEM)?;
    let child = Arc::try_new(CfsRunQueue::try_new().map_err(|_| SystemError::ENOMEM)?)
        .map_err(|_| SystemError::ENOMEM)?;
    let group = FairSchedEntity::try_new().map_err(|_| SystemError::ENOMEM)?;
    let competitor = FairSchedEntity::try_new().map_err(|_| SystemError::ENOMEM)?;
    let root = rq.cfs_rq();
    let one = first.sched_info().sched_entity();
    let two = second.sched_info().sched_entity();
    let dead_tasks = [Arc::downgrade(&first), Arc::downgrade(&second)];
    let dead_entities = [
        Arc::downgrade(&one),
        Arc::downgrade(&two),
        Arc::downgrade(&group),
        Arc::downgrade(&competitor),
    ];
    let dead_queues = [Arc::downgrade(&root), Arc::downgrade(&child)];
    let dead_rq = Arc::downgrade(&rq);
    let mut report = Report::default();
    let retired;
    {
        let (locked, guard) = rq.self_lock();
        root.force_mut().set_rq(Arc::downgrade(&rq));
        child.force_mut().set_rq(Arc::downgrade(&rq));
        group.force_mut().init_group(child.clone(), &root, None);
        one.force_mut().bind_task(&child, Some(&group));
        two.force_mut().bind_task(&child, Some(&group));
        competitor.force_mut().set_cfs(Arc::downgrade(&root));
        first.sched_info().set_on_cpu(Some(locked.cpu()));
        second.sched_info().set_on_cpu(Some(locked.cpu()));
        first
            .sched_info()
            .set_state(crate::process::ProcessState::Runnable);
        second
            .sched_info()
            .set_state(crate::process::ProcessState::Runnable);
        *first.sched_info().on_rq.lock_irqsave() = OnRq::Queued;
        *second.sched_info().on_rq.lock_irqsave() = OnRq::Queued;
        locked.set_current(Arc::downgrade(&first));
        // Private count only: add_nr_running() would alter global loadavg.
        locked.nr_running.store(2, Ordering::Relaxed);
        let base = 1_000_000_000;
        let slice = SYSCTL_SHCED_BASE_SLICE.load(Ordering::SeqCst);
        root.force_mut().min_vruntime = base;
        child.force_mut().min_vruntime = base;
        for entity in [&group, &one, &two, &competitor] {
            let entity = entity.force_mut();
            entity.vruntime = base;
            entity.deadline = base;
            entity.exec_start = locked.clock_task();
            entity.on_rq = OnRq::Queued;
        }
        // Input is a just-exhausted old request. Only the real production
        // update_deadline generates the new request; no post-switch vlag write.
        group.force_mut().vlag = base as i64;
        root.force_mut().account_entity_enqueue(&group);
        root.force_mut().set_current(Arc::downgrade(&group));
        root.force_mut().h_nr_running = 2;
        root.force_mut().update_deadline(&group);
        one.force_mut().deadline = base + slice;
        one.force_mut().vlag = (base - 1) as i64;
        two.force_mut().deadline = base + slice / 2;
        child.force_mut().account_entity_enqueue(&one);
        child.force_mut().account_entity_enqueue(&two);
        child.force_mut().h_nr_running = 2;
        child.force_mut().set_current(Arc::downgrade(&one));
        child.force_mut().inner_enqueue_entity(&two);
        report.old_token = group.vlag;
        report.deadline = group.deadline;
        report.setup = SCHED_FEATURES.contains(SchedFeature::RUN_TO_PARITY)
            && slice > 1
            && group.deadline > base
            && group.vlag != group.deadline as i64
            && root.nr_running == 1
            && child.nr_running == 2
            && root.entities.leftmost().is_none()
            && child
                .pick_next_entity()
                .is_some_and(|e| Arc::ptr_eq(&e, &two));

        // Same entry and input will remain unchanged in the production fix.
        let selected = locked.pick_next_task(first.clone());
        report.leaf_switch = Arc::ptr_eq(&selected, &second)
            && child.current().is_some_and(|e| Arc::ptr_eq(&e, &two))
            && root.current().is_some_and(|e| Arc::ptr_eq(&e, &group));
        report.observed_token = group.vlag;
        report.token_preserved = group.vlag == report.old_token
            && group.deadline == report.deadline
            && group.vruntime == base;

        // Always observe B even when old code has already refreshed the token.
        competitor.force_mut().load.weight = 2 * LoadWeight::NICE_0_LOAD;
        competitor.force_mut().load.inv_weight = 0;
        root.force_mut().update_deadline(&competitor);
        root.force_mut().account_entity_enqueue(&competitor);
        root.force_mut().inner_enqueue_entity(&competitor);
        report.b_eligible = root.entity_eligible(&group)
            && root.entity_eligible(&competitor)
            && competitor.deadline < group.deadline;
        report.edf_b = root
            .entities
            .pick_eevdf(Some(&group), |e| root.entity_eligible(e))
            .is_some_and(|e| Arc::ptr_eq(&e, &competitor));
        report.winner_b = root
            .pick_eevdf_entity(Some(&group))
            .is_some_and(|e| Arc::ptr_eq(&e, &competitor));

        // Retrieve each real prepared node; retain every payload owner until
        // after unlocking, including any unexpected retired PELT node chain.
        root.force_mut().set_current(Weak::new());
        child.force_mut().set_current(Weak::new());
        for queue in [&root, &child] {
            while let Some(entity) = queue.entities.leftmost() {
                queue.force_mut().inner_dequeue_entity(&entity);
            }
        }
        for (queue, entity) in [
            (&root, &group),
            (&root, &competitor),
            (&child, &one),
            (&child, &two),
        ] {
            queue.force_mut().account_entity_dequeue(entity);
            entity.force_mut().on_rq = OnRq::None;
        }
        retired = locked.cfs_pelt_queues.sweep(|entry| {
            entry
                .upgrade()
                .map_or(PeltDisposition::Retire, PeltDisposition::Recycle)
        });
        locked.set_current(Weak::new());
        locked.nr_running.store(0, Ordering::Relaxed);
        root.force_mut().h_nr_running = 0;
        child.force_mut().h_nr_running = 0;
        first.sched_info().set_on_cpu(None);
        second.sched_info().set_on_cpu(None);
        *first.sched_info().on_rq.lock_irqsave() = OnRq::None;
        *second.sched_info().on_rq.lock_irqsave() = OnRq::None;
        report.cleanup = retired.is_none()
            && root.entities.leftmost().is_none()
            && child.entities.leftmost().is_none()
            && root.nr_running == 0
            && child.nr_running == 0
            && root.load.weight == 0
            && child.load.weight == 0
            && root.pelt_node.is_some()
            && child.pelt_node.is_some()
            && !root.on_pelt_list
            && !child.on_pelt_list
            && [&group, &competitor, &one, &two]
                .iter()
                .all(|e| e.timeline_node.is_some());
        one.force_mut().set_cfs(Weak::new());
        two.force_mut().set_cfs(Weak::new());
        root.force_mut().set_rq(Weak::new());
        child.force_mut().set_rq(Weak::new());
        drop(guard);
    }
    drop(retired);
    drop((one, two, group, competitor, root, child, rq));
    drop((first, second)); // Normal PCB Drop; no bypass or escaped owner.
    report.cleanup &= dead_tasks.iter().all(|p| p.upgrade().is_none())
        && dead_entities.iter().all(|e| e.upgrade().is_none())
        && dead_queues.iter().all(|q| q.upgrade().is_none())
        && dead_rq.upgrade().is_none();
    Ok(report)
}
