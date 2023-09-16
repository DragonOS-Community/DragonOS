use alloc::{vec::Vec, boxed::Box};

use super::NapiDevice;

// napi队列内部存放的buffer结构

// 一个简易的napi的实现，这里的代码可以实现某个设备的关闭中断-轮询收包-开启中断的流程，但不能实现更复杂的机制，包括在收包队列在多个网卡之间的共享以及收包时间在多个网卡之间的轮转等
// 因为目前的协议栈还不支持底层设备向协议栈中提供packet，我们只能在NapiStruct中缓存收到的packet，同时在网卡驱动的receive方法中调用napi对应的receive方法
// struct NapiStruct<Buffer>{
//     napi_device: Box<dyn NapiDevice<Buffer = Buffer>>,
//     nid: u16,
// }

// impl<Buffer> NapiStruct<Buffer>{

// }
// // napi调度器，负责从poll_list中的设备收包并暂存起来
// struct NapiScheduler{
//     pkt_buffer: [Vec<u8>; 64] //暂时储存收到的数据包的缓冲区
//     // poll_list: Vec<NapiStruct> //需要轮询的设备列表
// }

// impl NapiScheduler{

//     pub fn napi_init(napi_struct: NapiStruct){
//         // self.poll_list.push(napi_struct);
//     }

//     pub fn napi_enable(napi_struct: NapiStruct){
        
//     }
    

// }
