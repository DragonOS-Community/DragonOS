==============================
RCU
==============================

RCU（Read-Copy Update）是一种面向读多写少场景的同步机制。本章介绍
DragonOS RCU 的核心原理、上下文模型、宽限期判定和组件职责。

.. toctree::
   :maxdepth: 1

   architecture
   segmented-callback-queues
