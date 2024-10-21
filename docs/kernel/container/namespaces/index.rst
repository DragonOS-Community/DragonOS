====================================
名称空间
====================================

DragonOS的namespaces目前支持pid_namespace和mnt_namespace 预计之后会继续完善
namespace是容器化实现过程中的重要组成部分

由于目前os是单用户，user_namespace为全局静态

.. toctree::
   :maxdepth: 1

   pid_namespace
   mnt_namespace
