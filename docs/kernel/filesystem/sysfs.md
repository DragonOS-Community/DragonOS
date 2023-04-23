# SysFS

:::{note}
本文作者：黄厅

Email: <huangting@DragonOS.org>
:::

## 1. SysFS和设备驱动模型

### 1.1. 设备、驱动、总线等彼此之间关系错综复杂

如果想让内核运行流畅，那就必须为每个模块编码实现这些功能。如此一来，内核将变得非常臃肿、冗余。而设备模型的理念即是将这些代码抽象成各模块共用的框架，这样不但代码简洁了，也可让设备驱动开发者摆脱这本让人头痛但又必不可少的一劫，将有限的精力放于设备差异性的实现。

设备模型恰是提供了一个模板，一个被证明过的最优的思路和流程，这减少了开发者设计过程中不必要的错误，也给以后的维护扫除了障碍。

### 1.2. sysfs是一个基于内存的文件系统，它的作用是将内核信息以文件的方式提供给用户程序使用。

​    sysfs可以看成与proc,devfs和devpty同类别的文件系统，该文件系统是虚拟的文件系统，可以更方便对系统设备进行管理。它可以产生一个包含所有系统硬件层次视图，与提供进程和状态信息的proc文件系统十分类似。sysfs把连接在系统上的设备和总线组织成为一个分级的文件，它们可以由用户空间存取，向用户空间导出内核的数据结构以及它们的属性。

## 2. DragosOS中的设备驱动模型

### 2.1 由设备和驱动构成基本元素

#### 2.1.1. 设备

```rust
/// @brief: 所有设备都应该实现该trait
pub trait Device: Any + Send + Sync + Debug {}
```

DragonOS采用全局设备管理器管理系统中所有的设备。

```rust
/// @brief Device管理器
#[derive(Debug, Clone)]
pub struct DeviceManager {
    devices: BTreeMap<IdTable, Arc<dyn Device>>, // 所有设备
    sys_info: Option<Arc<dyn IndexNode>>,  // sys information
}
```

#### 2.1.2. 驱动

```rust
/// @brief: 所有驱动驱动都应该实现该trait
pub trait Driver: Any + Send + Sync + Debug {}
```

同样的，驱动也使用全局的驱动管理器来管理

```rust
/// @brief: 驱动管理器
#[derive(Debug, Clone)]
pub struct DriverManager {
    drivers: BTreeMap<IdTable, Arc<dyn Driver>>, // 所有驱动
    sys_info: Option<Arc<dyn IndexNode>>, // sys information
}
```

### 2.2. 总线

总线属于设备的一种类型，同样需要驱动来初始化，同时由于总线的特殊性，使用全局的总线管理器来进行管理。

```rust
/// @brief: 总线驱动trait，所有总线驱动都应实现该trait
pub trait BusDriver: Driver {}

/// @brief: 总线设备trait，所有总线都应实现该trait
pub trait Bus: Device {}

/// @brief: 总线管理结构体
#[derive(Debug, Clone)]
pub struct BusManager {
    buses: BTreeMap<IdTable, Arc<dyn Bus>>,          // 总线设备表
    bus_drvs: BTreeMap<IdTable, Arc<dyn BusDriver>>, // 总线驱动表
    sys_info: Option<Arc<dyn IndexNode>>,            // 总线inode
}
```

可以看到，每个管理器中均存在sys_info，设备模型通过该成员与sysfs建立联系，sys_info指向sysfs中唯一的inode。对于device而言，对应sysfs下的devices文件夹，其他亦是如此。

## 3. 驱动开发如何进行

以平台总线platform为例，platform总线是一种虚拟总线，可以对挂载在其上的设备和驱动进行匹配，并驱动设备。该总线是一类设备，同时也是一类总线，编程时需要创建该设备实例，并为设备实例实现Device trait和Bus trait，以表明该结构是一类总线设备。同时，应该实现总线上的匹配规则，不同的总线匹配规则不同，该总线采用匹配表方式进行匹配，设备和驱动都应该存在一份匹配表，表示驱动支持的设备以及设备支持的驱动。

```rust
pub struct CompatibleTable(BTreeSet<&'static str>);
```

对于bus设备而言，需要调用bus_register，将bus注册进系统，并在sysfs中可视化。

```rust
/// @brief: 总线注册，将总线加入全局总线管理器中，并根据id table在sys/bus和sys/devices下生成文件夹
/// @parameter bus: Bus设备实体
/// @return: 成功:()   失败:DeviceError
pub fn bus_register<T: Bus>(bus: Arc<T>) -> Result<(), DeviceError> {
    BUS_MANAGER.add_bus(bus.get_id_table(), bus.clone());
    match sys_bus_register(&bus.get_id_table().to_name()) {
        Ok(inode) => {
            let _ = sys_bus_init(&inode);
            return device_register(bus);
        }
        Err(_) => Err(DeviceError::RegisterError),
    }
}
```

通过bus_register源码可知，该函数不仅在sysfs/bus下生成总线文件夹，同时内部调用device_register，该函数将总线加入设备管理器中，同时在sys/devices下生成设备文件夹。
