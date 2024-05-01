// include/linux/kobject.h
// lib/kobject_uevent.c

/*
    UEVENT_HELPER_PATH_LEN
    UEVENT_NUM_ENVP
    _KOBJECT_H_

Variable

    __randomize_layout

Enum

    kobject_action

Struct

    kobj_attribute
    kobj_type
    kobj_uevent_env
    kobject
    kset
    kset_uevent_ops

Function

    get_ktype
    kobject_name
    kset_get
    kset_put
    to_kset
*/
use crate::driver::base::kobject::KObject;
use alloc::string::String;
use alloc::vec::Vec;

pub mod kobject_uevent;

// https://code.dragonos.org.cn/xref/linux-6.1.9/lib/kobject_uevent.c?fi=kobject_uevent#457
// kobject_action
pub enum KobjectAction {
        KOBJADD, 
        KOBJREMOVE, //Kobject（或上层数据结构）的添加/移除事件
        KOBJCHANGE, //Kobject（或上层数据结构）的状态或者内容发生改变; 如果设备驱动需要上报的事件不再上面事件的范围内，或者是自定义的事件，可以使用该event，并携带相应的参数。
        KOBJMOVE, //Kobject（或上层数据结构）更改名称或者更改Parent（意味着在sysfs中更改了目录结构）
        KOBJONLINE,
        KOBJOFFLINE, //Kobject（或上层数据结构）的上线/下线事件，其实是是否使能
        KOBJBIND,
        KOBJUNBIND,
    }

/*
    @parament: 
    
    envp，指针数组，用于保存每个环境变量的地址，最多可支持的环境变量数量为UEVENT_NUM_ENVP。

    envp_idx，用于访问环境变量指针数组的index。

    buf，保存环境变量的buffer，最大为UEVENT_BUFFER_SIZE。

    buflen，访问buf的变量。

*/

//https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/kobject.h#31

pub const UEVENT_NUM_ENVP :usize = 64;
pub const UEVENT_BUFFER_SIZE:usize= 2048;
pub const UEVENT_HELPER_PATH_LEN:usize = 256;

/// Represents the environment for handling kernel object uevents.
/*
    envp，指针数组，用于保存每个环境变量的地址，最多可支持的环境变量数量为UEVENT_NUM_ENVP。

    envp_idx，用于访问环境变量指针数组的index。

    buf，保存环境变量的buffer，最大为UEVENT_BUFFER_SIZE。

    buflen，访问buf的变量。

*/
// 表示一个待发送的uevent
pub struct KobjUeventEnv {
    argv: Vec<Option<String>>,
    envp: Vec<Option<String>>,
    envp_idx: usize,
    buf: Vec<Option<String>>,
    buflen: usize,
}

//kset_uevent_ops是为kset量身订做的一个数据结构，里面包含filter和uevent两个回调函数，用处如下： 
/*
    filter，当任何Kobject需要上报uevent时，它所属的kset可以通过该接口过滤，阻止不希望上报的event，从而达到从整体上管理的目的。

    name，该接口可以返回kset的名称。如果一个kset没有合法的名称，则其下的所有Kobject将不允许上报uvent

    uevent，当任何Kobject需要上报uevent时，它所属的kset可以通过该接口统一为这些event添加环境变量。因为很多时候上报uevent时的环境变量都是相同的，因此可以由kset统一处理，就不需要让每个Kobject独自添加了。

*/
