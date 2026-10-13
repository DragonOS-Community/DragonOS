//! Tasklet bottom halves use one preborn queue node per tasklet.
//! Schedule/dequeue/requeue never allocate; callbacks retain their own contracts.
use crate::{
    cgroup::memory::{accounting::KernelByteKind, object::ObjectAccount, owned::OwnedBox},
    exception::softirq::{softirq_vectors, SoftirqNumber, SoftirqVec},
    libs::spinlock::SpinLock,
    mm::{allocation::MemoryAllocationError, percpu::PerCpu},
    process::preempt::PreemptGuard,
    smp::core::smp_get_processor_id,
};
use alloc::{boxed::Box, sync::Arc};
use core::{
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicBool, Ordering},
};
use system_error::SystemError;

struct Node {
    payload: Option<Arc<Tasklet>>,
    next: Option<QueueNode>,
}
enum QueueNode {
    Bootstrap(Box<Node>),
    Accounted(OwnedBox<Node>),
}
impl Deref for QueueNode {
    type Target = Node;
    fn deref(&self) -> &Node {
        match self {
            Self::Bootstrap(v) => v,
            Self::Accounted(v) => v,
        }
    }
}
impl DerefMut for QueueNode {
    fn deref_mut(&mut self) -> &mut Node {
        match self {
            Self::Bootstrap(v) => v,
            Self::Accounted(v) => v,
        }
    }
}
impl Drop for Node {
    fn drop(&mut self) {
        let mut next = self.next.take();
        while let Some(mut node) = next {
            next = node.next.take();
            drop(node);
        }
    }
}
struct TaskletStateData {
    is_scheduled: AtomicBool,
    is_running: AtomicBool,
    callback: Arc<dyn TaskletFunc>,
    data: usize,
    data_obj: Option<Arc<dyn TaskletData>>,
    spare: SpinLock<Option<QueueNode>>,
}
enum Body {
    Bootstrap(Box<TaskletStateData>),
    Accounted(OwnedBox<TaskletStateData>),
}
pub struct Tasklet {
    body: Body,
}
impl Tasklet {
    fn body(&self) -> &TaskletStateData {
        match &self.body {
            Body::Bootstrap(v) => v,
            Body::Accounted(v) => v,
        }
    }
    /// Explicit bootstrap/driver metadata, not the interrupted task's policy.
    /// The extra body/node allocations occur once, never at schedule time.
    pub fn new<F>(callback: F, data: usize, data_obj: Option<Arc<dyn TaskletData>>) -> Arc<Self>
    where
        F: TaskletFunc + 'static,
    {
        Arc::new(Self {
            body: Body::Bootstrap(Box::new(TaskletStateData {
                is_scheduled: AtomicBool::new(false),
                is_running: AtomicBool::new(false),
                callback: Arc::new(callback),
                data,
                data_obj,
                spare: SpinLock::new(Some(QueueNode::Bootstrap(Box::new(Node {
                    payload: None,
                    next: None,
                })))),
            })),
        })
    }
    pub(crate) fn try_new_for<F>(
        callback: F,
        data: usize,
        data_obj: Option<Arc<dyn TaskletData>>,
        creator: &Arc<ObjectAccount>,
    ) -> Result<Arc<Self>, MemoryAllocationError>
    where
        F: TaskletFunc + 'static,
    {
        struct Callback<F> {
            body: OwnedBox<F>,
        }
        impl<F: TaskletFunc> TaskletFunc for Callback<F> {
            fn call(&self, data: usize, obj: Option<Arc<dyn TaskletData>>) {
                self.body.call(data, obj);
            }
        }
        let body =
            OwnedBox::<TaskletStateData>::prepare(creator, KernelByteKind::SlabUnreclaimable)?
                .try_allocate()
                .map_err(|_| SystemError::ENOMEM)?;
        let closure = OwnedBox::prepare(creator, KernelByteKind::SlabUnreclaimable)?
            .try_init(callback)
            .map_err(|_| SystemError::ENOMEM)?;
        let callback = Arc::try_new(Callback { body: closure }).map_err(|_| SystemError::ENOMEM)?;
        let spare = OwnedBox::prepare(creator, KernelByteKind::SlabUnreclaimable)?
            .try_init(Node {
                payload: None,
                next: None,
            })
            .map_err(|_| SystemError::ENOMEM)?;
        let body = body.init(TaskletStateData {
            is_scheduled: AtomicBool::new(false),
            is_running: AtomicBool::new(false),
            callback,
            data,
            data_obj,
            spare: SpinLock::new(Some(QueueNode::Accounted(spare))),
        });
        Ok(Arc::try_new(Self {
            body: Body::Accounted(body),
        })
        .map_err(|_| SystemError::ENOMEM)?)
    }
}
pub trait TaskletData: Send + Sync {}
impl<T: Send + Sync> TaskletData for T {}
pub trait TaskletFunc: Send + Sync {
    fn call(&self, data: usize, data_obj: Option<Arc<dyn TaskletData>>);
}
impl<F> TaskletFunc for F
where
    F: Fn(usize, Option<Arc<dyn TaskletData>>) + Send + Sync,
{
    fn call(&self, data: usize, data_obj: Option<Arc<dyn TaskletData>>) {
        (self)(data, data_obj);
    }
}
static TASKLET_QUEUE: [SpinLock<Option<QueueNode>>; PerCpu::MAX_CPU_NUM as usize] =
    [const { SpinLock::new(None) }; PerCpu::MAX_CPU_NUM as usize];
fn local_queue() -> &'static SpinLock<Option<QueueNode>> {
    &TASKLET_QUEUE[smp_get_processor_id().data() as usize]
}
pub fn tasklet_schedule(t: &Arc<Tasklet>) {
    // A task-context producer may migrate between selecting its queue and
    // raising the local softirq. Both operations must refer to one CPU.
    let _preempt = PreemptGuard::new();
    let body = t.body();
    if body
        .is_scheduled
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    let mut head = local_queue().lock_irqsave();
    let mut spare = body.spare.lock_irqsave();
    let mut node = spare.take().expect("unscheduled tasklet owns its spare");
    assert!(node.payload.is_none() && node.next.is_none());
    node.payload = Some(t.clone());
    node.next = head.take();
    *head = Some(node);
    drop(spare);
    drop(head);
    softirq_vectors().raise_softirq(SoftirqNumber::TASKLET);
}
#[derive(Debug, Default)]
struct TaskletSoftirq;
impl SoftirqVec for TaskletSoftirq {
    fn run(&self) {
        // A finite batch: requeued/new nodes enter live, never this processing
        // chain. Running on another CPU cannot make this callback spin forever.
        let mut processing = local_queue().lock_irqsave().take();
        while let Some(mut node) = processing {
            processing = node.next.take();
            let t = node
                .payload
                .take()
                .expect("queued tasklet owns its payload");
            let body = t.body();
            if body
                .is_running
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                .is_err()
            {
                // Keep scheduled=true and the original node. Public schedule's
                // CAS would reject this necessary cross-CPU requeue.
                node.payload = Some(t);
                let mut head = local_queue().lock_irqsave();
                node.next = head.take();
                *head = Some(node);
                drop(head);
                softirq_vectors().raise_softirq(SoftirqNumber::TASKLET);
                continue;
            }
            {
                let mut spare = body.spare.lock_irqsave();
                assert!(spare.is_none());
                *spare = Some(node);
            }
            // Spare is already available when a callback/remote CPU schedules.
            body.is_scheduled.store(false, Ordering::Release);
            body.callback.call(body.data, body.data_obj.clone());
            body.is_running.store(false, Ordering::Release);
            // Final payload/body/callback references are destroyed lock-free.
        }
    }
}
#[inline(never)]
pub fn tasklet_init() -> Result<(), SystemError> {
    let handler = Arc::new(TaskletSoftirq);
    softirq_vectors().register_softirq(SoftirqNumber::TASKLET, handler)?;
    Ok(())
}
