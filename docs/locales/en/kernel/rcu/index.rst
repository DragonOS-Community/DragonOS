==============================
RCU
==============================

RCU (Read-Copy Update) is a synchronization mechanism designed for
read-mostly workloads. This chapter describes the core principles, context
model, grace-period detection, and component responsibilities of DragonOS
RCU.

.. toctree::
   :maxdepth: 1

   architecture
   segmented-callback-queues
   srcu-design
