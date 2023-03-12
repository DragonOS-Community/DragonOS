# DragonOS虚拟文件系统概述

## 简介

&emsp;&emsp;DragonOS的虚拟文件系统是内核中的一层适配器，为用户程序（或者是系统程序）提供了通用的文件系统接口。同时对内核中的不同文件系统提供了统一的抽象。各种具体的文件系统可以挂载到VFS的框架之中。

&emsp;&emsp;与VFS相关的系统调用有open(), read(), write(), create()等。

## **TODO**

&emsp;&emsp;VFS的设计与实现讲解