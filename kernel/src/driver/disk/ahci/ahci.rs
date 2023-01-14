use core::{default, ffi::c_void, ptr::null_mut};

use alloc::{
    boxed::Box,
    vec::Vec,
};

use crate::{
    include::bindings::bindings::{
        ahci_check_complete, ahci_request_packet_t, complete,
        completion, completion_init, get_completion,
    },
    kBUG,
    libs::spinlock::RawSpinlock,
};

///  achi请求包
pub struct AhciRequestPacket {
    pub ahci_ctrl_num: u8,
    pub port_num: u8,
}


impl AhciRequestPacket {
    pub fn new() -> Self {
        return AhciRequestPacket {
            ..Default::default()
        };
    }
}

impl Default for AhciRequestPacket {
    fn default() -> Self {
        AhciRequestPacket {
            ahci_ctrl_num: 0,
            port_num: Default::default(),
        }
    }
}

/// io请求包
pub struct BlockDeviceRequestPacket<T> {
    pub cmd: u8,
    pub lba_start: u64,
    pub count: u32,
    pub buffer_vaddr: u64,
    pub device_type: u8, // 0: ahci
    pub end_handler: ::core::option::Option<
        unsafe extern "C" fn(num: ::core::ffi::c_ulong, arg: ::core::ffi::c_ulong),
    >,
    pub private_ahci_request_packet: T,
    pub status: *mut completion,
}

impl<AhciRequestPacket> BlockDeviceRequestPacket<AhciRequestPacket> {
    pub fn new(
        ahci_request_packet: AhciRequestPacket,
    ) -> BlockDeviceRequestPacket<AhciRequestPacket> {

        let cmpl: *mut completion =unsafe {&mut get_completion()}; 
        unsafe {
            completion_init(cmpl);
        }
        return BlockDeviceRequestPacket {
            cmd: Default::default(),
            lba_start: Default::default(),
            count: Default::default(),
            buffer_vaddr: Default::default(),
            device_type: Default::default(),
            end_handler: Default::default(),
            private_ahci_request_packet: ahci_request_packet,
            status: cmpl,
        };
    }

}


struct RequestQueue {
    lock: RawSpinlock,
    waiting_queue: Vec<BlockDeviceRequestPacket<AhciRequestPacket>>,
    processing_queue: Vec<BlockDeviceRequestPacket<AhciRequestPacket>>,
}

impl RequestQueue {
    pub fn new() -> RequestQueue {
        RequestQueue {
            lock: RawSpinlock::INIT,
            waiting_queue: Vec::new(),
            processing_queue: Vec::new(),
        }
    }

    ///  @brief 将请求包插入等待队列中
    pub fn push_waiting_queue(
        &mut self,
        ahci_request_packet: BlockDeviceRequestPacket<AhciRequestPacket>,
    ) {
        self.waiting_queue.push(ahci_request_packet);
    }

    ///  @brief 将请求包从正在执行队列中弹出
    pub fn pop_waiting_queue(&mut self) -> Option<BlockDeviceRequestPacket<AhciRequestPacket>> {
        let mut res: Option<BlockDeviceRequestPacket<AhciRequestPacket>> = None;
        if self.waiting_queue.len() == 0 {
            return res;
        }
        res = Some(self.waiting_queue.remove(0));
        return res;
    }

    ///  @brief 将请求包插入正在执行队列中
    pub fn push_processing_queue(
        &mut self,
        ahci_request_packet: BlockDeviceRequestPacket<AhciRequestPacket>,
    ) {
        self.processing_queue.push(ahci_request_packet);
    }

    ///  @brief 将请求包从正在执行队列中弹出
    pub fn pop_processing_queue(&mut self) -> Option<BlockDeviceRequestPacket<AhciRequestPacket>> {
        let mut res: Option<BlockDeviceRequestPacket<AhciRequestPacket>> = None;
        if self.processing_queue.len() == 0 {
            return res;
        }
        res = Some(self.processing_queue.remove(0));
        return res;
    }
}

pub struct SchedulerIO {
    io_queue: Vec<&'static mut RequestQueue>,
}

impl SchedulerIO {
    pub fn new() -> SchedulerIO {
        return SchedulerIO {
            io_queue: Default::default(),
        };
    }
}
pub static mut IO_SCHEDULER_PTR: *mut SchedulerIO = null_mut();

#[inline]
pub fn __get_cfs_scheduler() -> &'static mut SchedulerIO {
    return unsafe { IO_SCHEDULER_PTR.as_mut().unwrap() };
}

/// @brief 初始化io调度器
#[no_mangle]
pub unsafe extern "C" fn io_schduler_init() {
    if IO_SCHEDULER_PTR.is_null() {
        IO_SCHEDULER_PTR = Box::leak(Box::new(SchedulerIO::new()));
    } else {
        kBUG!("Try to init IO Scheduler twice.");
        panic!("Try to init IO Scheduler twice.");
    }
}

#[no_mangle]
/// @brief 处理请求
pub extern "C" fn address_requests() {
    let io_scheduler = __get_cfs_scheduler();
    let mut res: i32;
    let mut delete_index: Vec<usize> = Vec::new();
    //FIXME 暂时只考虑了一个io队列的情况

    loop {
        //检查 正在执行的请求包
        for (index, packet) in &mut (io_scheduler.io_queue[0].processing_queue)
            .iter_mut()
            .enumerate()
        {
            unsafe {
                res = ahci_check_complete(
                    packet.private_ahci_request_packet.port_num,
                    packet.private_ahci_request_packet.ahci_ctrl_num,
                    null_mut(),
                );
            }
            if res == 0 {
                //将状态设置为完成
                unsafe { complete(packet.status) };
                delete_index.push(index);
            }
        }
        for i in &delete_index {
            io_scheduler.io_queue[0].processing_queue.remove(*i);
        }
        //将等待中的请求包插入
        for _ in 0..2 {
            if !io_scheduler.io_queue[0].lock.is_locked() {
                io_scheduler.io_queue[0].lock.lock();
                if io_scheduler.io_queue[0].processing_queue.len() == 3
                    || io_scheduler.io_queue[0].waiting_queue.len() == 0
                {
                    break;
                }
                let packet = io_scheduler.io_queue[0].pop_waiting_queue().unwrap();
                io_scheduler.io_queue[0].push_processing_queue(packet);
                io_scheduler.io_queue[0].lock.unlock();
            }
        }
    }
}

/// @brief 将c中的ahci_request_packet_t转换成rust中的BlockDeviceRequestPacket<AhciRequestPacket>
pub fn crate_ahci_request(ahci_request_packet:&ahci_request_packet_t)->BlockDeviceRequestPacket<AhciRequestPacket>{
        let cmpl: *mut completion =unsafe {
        &mut get_completion()
    }; 
    unsafe {
        completion_init(cmpl);
    }
    //将c的ahci_request_packet_t 转换成rust BlockDeviceRequestPacket<AhciRequestPacket>
    let ahci_packet = AhciRequestPacket{
        ahci_ctrl_num:ahci_request_packet.ahci_ctrl_num,
        port_num:ahci_request_packet.port_num
    };
    let packet =  BlockDeviceRequestPacket{
        private_ahci_request_packet:ahci_packet,
        buffer_vaddr:ahci_request_packet.blk_pak.buffer_vaddr,
        cmd:ahci_request_packet.blk_pak.cmd,
        count:ahci_request_packet.blk_pak.count,
        device_type:ahci_request_packet.blk_pak.device_type,
        end_handler:ahci_request_packet.blk_pak.end_handler,
        lba_start:ahci_request_packet.blk_pak.LBA_start,
        status: cmpl,
    };
    return  packet;
}

#[no_mangle]
/// @brief 将ahci的io请求插入等待队列中
pub extern  "C"  fn ahci_push_request(ahci_request_packet: &ahci_request_packet_t ){
    let packet = crate_ahci_request(ahci_request_packet);
    let io_scheduler = __get_cfs_scheduler();
    io_scheduler.io_queue[0].push_waiting_queue(packet);
}