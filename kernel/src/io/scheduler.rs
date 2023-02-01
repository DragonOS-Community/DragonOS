use core::{ptr::null_mut, sync::atomic::compiler_fence};

use alloc::{boxed::Box, string::String, vec::Vec};
use x86_64::registers::debug;

use crate::{
    arch::{asm::current::current_pcb, mm::barrier::mfence, sched::sched},
    include::bindings::bindings::{
        ahci_check_complete, ahci_query_disk, ahci_request_packet_t, block_device_request_packet,
        clock, clock_t, complete, completion, completion_alloc, process_control_block,
        process_wakeup, process_wakeup_immediately, schedule_timeout_ms, usleep,
        wait_for_completion, PF_NEED_SCHED, PROC_RUNNING,
    },
    kBUG, kdebug,
    libs::spinlock::RawSpinlock,
};
#[derive(Debug)]

///  achi请求包
pub struct AhciRequestPacket {
    pub ahci_ctrl_num: u8,
    pub port_num: u8,
    pub slot: i8,
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
            slot: -1,
        }
    }
}
#[derive(Debug)]

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
        let cmpl: *mut completion = unsafe { completion_alloc() };

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

    ///  @brief 将已完成请求包从执行队列中弹出
    pub fn pop_finished_packets(&mut self) {
        if self.processing_queue.len() != 0 {
            compiler_fence(core::sync::atomic::Ordering::SeqCst);
            //将状态设置为完成
            mfence();
            let filter = |packet: &mut BlockDeviceRequestPacket<AhciRequestPacket>| {
                let mut res = unsafe {
                    ahci_check_complete(
                        packet.private_ahci_request_packet.port_num,
                        packet.private_ahci_request_packet.ahci_ctrl_num,
                        packet.private_ahci_request_packet.slot,
                        null_mut(),
                    )
                };
                if res == 0 {
                    unsafe {
                        compiler_fence(core::sync::atomic::Ordering::SeqCst);
                        // kdebug!("{:?}\n", packet);
                        complete(packet.status);
                        compiler_fence(core::sync::atomic::Ordering::SeqCst);
                    }
                    return true;
                }
                return false;
            };
            self.processing_queue.drain_filter(filter);
            mfence();
            compiler_fence(core::sync::atomic::Ordering::SeqCst);
        }
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
pub fn __get_io_scheduler() -> &'static mut SchedulerIO {
    return unsafe { IO_SCHEDULER_PTR.as_mut().unwrap() };
}

/// @brief 初始化io调度器
#[no_mangle]
pub unsafe extern "C" fn io_scheduler_init_rust() {
    if IO_SCHEDULER_PTR.is_null() {
        IO_SCHEDULER_PTR = Box::leak(Box::new(SchedulerIO::new()));
        create_io_queue();
    } else {
        kBUG!("Try to init IO Scheduler twice.");
        panic!("Try to init IO Scheduler twice.");
    }
}

/// @brief 初始化io请求队列
#[no_mangle]
pub extern "C" fn create_io_queue() {
    let io_scheduler = __get_io_scheduler();
    io_scheduler
        .io_queue
        .push(Box::leak(Box::new(RequestQueue::new())));
}
#[derive(Debug)]
struct log {
    t: clock_t,
    dosomething: String,
}
#[no_mangle]
/// @brief 处理请求 （守护线程运行）
pub extern "C" fn io_scheduler_address_requests() {
    let io_scheduler = __get_io_scheduler();

    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    let mut looplog: Vec<log> = Vec::new();
    //FIXME 暂时只考虑了一个io队列的情况
    loop {
        compiler_fence(core::sync::atomic::Ordering::SeqCst);

        if io_scheduler.io_queue[0].waiting_queue.len() == 0
            && io_scheduler.io_queue[0].processing_queue.len() == 0
        {
            // kdebug!("sched out");
            unsafe {
                compiler_fence(core::sync::atomic::Ordering::SeqCst);
                // schedule_timeout_ms(5);
                current_pcb().flags |= PF_NEED_SCHED as u64;
                sched();
                compiler_fence(core::sync::atomic::Ordering::SeqCst);
            }
        }
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
        //请不要修改下面三个循环的顺序

        // let begin: u64 = unsafe { clock().try_into().unwrap() };
        // kdebug!("{}",begin);
        //将等待中的请求包插入
        let size = io_scheduler.io_queue[0].waiting_queue.len();
        for i in 0..16 {
            // let begin: u64 = unsafe { clock().try_into().unwrap() };

            if i >= size || io_scheduler.io_queue[0].processing_queue.len() == 16 {
                break;
            }
            compiler_fence(core::sync::atomic::Ordering::SeqCst);
            // if !io_scheduler.io_queue[0].lock.is_locked() {
            io_scheduler.io_queue[0].lock.lock();
            let mut packet = io_scheduler.io_queue[0].pop_waiting_queue().unwrap();
            //分发请求包
            let mut ahci_packet: ahci_request_packet_t = convert_c_ahci_request(&packet);
            let mut ret_slot: i8 = -1;
            unsafe {
                compiler_fence(core::sync::atomic::Ordering::SeqCst);
                ahci_query_disk(&mut ahci_packet, &mut ret_slot);
                compiler_fence(core::sync::atomic::Ordering::SeqCst);
            }
            packet.private_ahci_request_packet.slot = ret_slot;
            io_scheduler.io_queue[0].push_processing_queue(packet);
            io_scheduler.io_queue[0].lock.unlock();
            // }

            // looplog.push(log {
            //     t: ((unsafe { clock() } - begin) ).try_into().unwrap(),
            //     dosomething: String::from("push_processing_queue"),
            // });

            // kdebug!("{:?} ", ahci_packet,);

            // let t: u64 = ((unsafe { clock() } - begin) / 1000).try_into().unwrap();
            // if t > 0 {
            //     kdebug!("{:?}", ahci_packet);
            // }

            compiler_fence(core::sync::atomic::Ordering::SeqCst);
        }
        compiler_fence(core::sync::atomic::Ordering::SeqCst);

        io_scheduler.io_queue[0].lock.lock();
        io_scheduler.io_queue[0].pop_finished_packets();
        io_scheduler.io_queue[0].lock.unlock();
        mfence();
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
    }
}

pub fn convert_c_ahci_request(
    pakcet: &BlockDeviceRequestPacket<AhciRequestPacket>,
) -> ahci_request_packet_t {
    let ahci_packet: ahci_request_packet_t = ahci_request_packet_t {
        ahci_ctrl_num: pakcet.private_ahci_request_packet.ahci_ctrl_num,
        port_num: pakcet.private_ahci_request_packet.port_num,
        blk_pak: block_device_request_packet {
            LBA_start: pakcet.lba_start,
            cmd: pakcet.cmd,
            buffer_vaddr: pakcet.buffer_vaddr,
            count: pakcet.count,
            device_type: pakcet.device_type,
            end_handler: pakcet.end_handler,
        },
    };
    return ahci_packet;
}

/// @brief 将c中的ahci_request_packet_t转换成rust中的BlockDeviceRequestPacket<AhciRequestPacket>
pub fn create_ahci_request(
    ahci_request_packet: &ahci_request_packet_t,
) -> BlockDeviceRequestPacket<AhciRequestPacket> {
    let cmpl: *mut completion = unsafe { completion_alloc() };
    let ahci_packet = AhciRequestPacket {
        ahci_ctrl_num: ahci_request_packet.ahci_ctrl_num,
        port_num: ahci_request_packet.port_num,
        slot: -1,
    };
    let packet = BlockDeviceRequestPacket {
        private_ahci_request_packet: ahci_packet,
        buffer_vaddr: ahci_request_packet.blk_pak.buffer_vaddr,
        cmd: ahci_request_packet.blk_pak.cmd,
        count: ahci_request_packet.blk_pak.count,
        device_type: ahci_request_packet.blk_pak.device_type,
        end_handler: ahci_request_packet.blk_pak.end_handler,
        lba_start: ahci_request_packet.blk_pak.LBA_start,
        status: cmpl,
    };

    return packet;
}

#[no_mangle]
/// @brief 将ahci的io请求插入等待队列中
pub extern "C" fn ahci_push_request(ahci_request_packet: &ahci_request_packet_t) {
    let packet = create_ahci_request(ahci_request_packet);
    let io_scheduler = __get_io_scheduler();
    let status = packet.status;
    io_scheduler.io_queue[0].lock.lock();
    io_scheduler.io_queue[0].push_waiting_queue(packet);
    io_scheduler.io_queue[0].lock.unlock();
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    unsafe {
        wait_for_completion(status);
    }
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
}
