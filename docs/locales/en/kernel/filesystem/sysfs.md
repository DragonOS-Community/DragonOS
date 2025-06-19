:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/filesystem/sysfs.md

- Translation time: 2025-05-19 01:41:50

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# SysFS

:::{note}
Author: Huang Ting

Email: <huangting@DragonOS.org>
:::

## 1. SysFS and Device Driver Model

### 1.1. The relationship between devices, drivers, buses, etc., is complex

&emsp;&emsp;If you want the kernel to run smoothly, you must code these functionalities for each module. This will make the kernel very bloated and redundant. The idea of the device model is to abstract these codes into a shared framework for all modules. This not only makes the code concise, but also allows device driver developers to avoid the headache of this essential but burdensome task, and focus their limited energy on implementing the differences of the devices.

&emsp;&emsp;The device model provides a template, an optimal approach and process that has been proven. This reduces unnecessary errors during the development process and clears the way for future maintenance.

### 1.2. sysfs is a memory-based file system, its role is to provide kernel information in the form of files for user programs to use.

&emsp;&emsp;sysfs can be seen as a file system similar to proc, devfs, and devpty. This file system is virtual and can make it easier to manage system devices. It can generate a hierarchical view of all system hardware, similar to the proc file system that provides process and status information. sysfs organizes the devices and buses connected to the system into a hierarchical file structure, which can be accessed from user space, exporting kernel data structures and their attributes to user space.

## 2. Device Driver Model in DragonOS

### 2.1. The basic elements are composed of devices and drivers

#### 2.1.1. Device

```rust
/// @brief: 所有设备都应该实现该trait
pub trait Device: Any + Send + Sync + Debug {}
```

&emsp;&emsp;DragonOS uses a global device manager to manage all devices in the system.

```rust
/// @brief Device管理器
#[derive(Debug, Clone)]
pub struct DeviceManager {
    devices: BTreeMap<IdTable, Arc<dyn Device>>, // 所有设备
    sys_info: Option<Arc<dyn IndexNode>>,  // sys information
}
```

#### 2.1.2. Driver

```rust
/// @brief: 所有驱动驱动都应该实现该trait
pub trait Driver: Any + Send + Sync + Debug {}
```

&emsp;&emsp;Similarly, drivers also use a global driver manager for management.

```rust
/// @brief: 驱动管理器
#[derive(Debug, Clone)]
pub struct DriverManager {
    drivers: BTreeMap<IdTable, Arc<dyn Driver>>, // 所有驱动
    sys_info: Option<Arc<dyn IndexNode>>, // sys information
}
```

### 2.2. Bus

&emsp;&emsp;Bus is a type of device, and it also needs a driver to initialize. Due to the special nature of buses, a global bus manager is used for management.

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

&emsp;&emsp;As can be seen, each manager contains a sys_info. The device model establishes a connection with sysfs through this member, and sys_info points to the unique inode in sysfs. For a device, it corresponds to the devices folder under sysfs, and the same applies to other components.

## 3. How to Develop Drivers

&emsp;&emsp;Taking the platform bus as an example, the platform bus is a virtual bus that can match devices and drivers mounted on it and drive the devices. This bus is a type of device and also a type of bus. When programming, you need to create an instance of this device and implement the Device trait and Bus trait for the device instance to indicate that this structure is a bus device. At the same time, the matching rules on the bus should be implemented. Different buses have different matching rules. This bus uses a matching table for matching, and both devices and drivers should have a matching table, indicating the devices supported by the driver and the drivers supported by the device.

```rust
pub struct CompatibleTable(BTreeSet<&'static str>);
```

&emsp;&emsp;For a bus device, you need to call bus_register to register the bus into the system and visualize it in sysfs.

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

&emsp;&emsp;From the source code of bus_register, we can see that this function not only generates a bus folder under sysfs/bus, but also internally calls device_register. This function adds the bus to the device manager and generates a device folder under sys/devices.
