# APIs Related to the Completely Fair Scheduler

CFS (Completely Fair Scheduler), as the name suggests, aims to schedule fairly. It is one of the mainline schedulers and a typical O(1) scheduler.

## Structures

- `CompletelyFairScheduler`: implements the `Scheduler` trait and is the main driver of the CFS algorithm.
- `FairSchedEntity`
  - **Important fields**
    - `cfs_rq`: the CFS run queue this entity belongs to.
    - `my_cfs_rq`: an `Option`. `None` when the entity is a single process; if the entity is a group, it must point to that group's private run queue. That queue can nest further and form a tree.
    - `pcb`: the `PCB` of this entity. If the entity is a group, this `Weak` pointer does not point to anything.

`FairSchedEntity` is the most important CFS structure. It represents one scheduling entity: a process, a group, or a user. In the CFS queue it is always just one entity. Upper layers can group processes into one entity (group scheduling) without changing the algorithm.

CFS is organized as a **tree**. Each entity is a node in a `cfs_rq`. If the entity is not a single process (for example a process group), it keeps its own `cfs_rq`. After nesting, every leaf is a single process. Later documents explain CFS around this tree.

See the source for the full field list. The important fields of the run queue are:

- `CfsRunQueue`: the queue that holds `FairSchedEntity` objects. It can hang under the per-CPU `CpuRunQueue`, or as a child of another `FairSchedEntity`.
  - **Important fields**
    - `entities`: red-black tree of scheduling entities
    - `current`: the entity currently running
