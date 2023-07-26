use alloc::{boxed::Box,collections::LinkedList,sync::{Arc,Mutex},
    string::{String,ToString},vec::Vec,
};
use core::{fmt::Arguments,ptr::null_mut,ffi::c_void};
use crate::{
    process::{ProcessControlBlock,ProcessFlags,PCB_NAME_LEN,ProcessState},
    arch::asm::current::current_pcb,sched::sched,
    libs::{
        spinlock::{SpinLock,SpinLockGuard},
    },
    include::bindings::bindings::{process_wakeup_immediately, kernel_thread,process_do_exit,process_set_pcb_name, get_rflags},
    kdebug, kinfo,
};


static KTHREAD_CREATE_LIST: SpinLock<LinkedList<Arc<KThreadCreateInfo>>> =SpinLock::new(LinkedList::new());

static KTHREADD_PCB: Arc<SpinLock<Option<ProcessControlBlock>>> =Arc::new(SpinLock::new(None));

#[derive(Clone)]
struct KThreadCreateInfo {
    thread_fn: fn(*mut u8) -> i32,
    data: *mut u8,
    node: i32,
    result: Option<Arc<ProcessControlBlock>>,
}

enum KThreadBits {
    IsPerCpu = 0,
    ShouldStop,
    ShouldPark,
}

struct KThreadInfo {
    flags: i32,
    worker_private: Option<KThreadCreateInfo>,
    result: i32,
    exited: bool,
    full_name: Option<String>,
    thread_fn: fn(*mut u8) -> isize,
}

fn to_kthread(pcb: &Arc<ProcessControlBlock>) -> &KThreadInfo {
    let inner = pcb.inner.read();
    let flags = inner.flags;
    assert!(flags.contains(ProcessFlags::KTHREAD), "to_kthread: not a kthread");

    let worker_private = inner.worker_private.unwrap();
    unsafe { &*(worker_private as *const KThreadInfo) } 
}



fn __kthread_create_on_node(thread_fn: fn(*mut c_void) -> isize, 
                        data: *mut c_void,
                        name: &str) -> Result<Arc<ProcessControlBlock>, Error> {

    let create_info = Arc::new(KThreadCreateInfo::new(thread_fn, data));
    
    let mut list = KTHREAD_CREATE_LIST.lock();
    list.push(create_info.clone());
    drop(list);

    let kthreadd_pcb = loop {
        let tcb = KTHREADD_PCB.lock();
        if tcb.is_some() {
            break tcb.clone().unwrap();
        }
    };
    drop(kthreadd_pcb);

    // 唤醒 kthreadd

    let new_tcb = create_info.thread_fn(create_info.data);

    if let Some(err) = new_tcb.get_error() {
        return Err(err); 
    }

    let mut new_tcb = new_tcb.unwrap();
    new_tcb.set_name(name); 

    Ok(new_tcb)
}





pub fn kthread(create: &mut KThreadCreateInfo) -> isize {
    let thread_fn = create.thread_fn;
    let data = create.data;

    let mut retval = 0;

    let self_kthread = current_kthread();

    self_kthread.thread_fn = thread_fn;
    self_kthread.data = data;

    // 设置当前进程为不可被打断
    current_pcb().state.set(ProcessState::UNINTERRUPTIBLE);

    // 将当前pcb返回给创建者
    create.result = current_task();

    // 设置当前进程不是运行状态
    current_pcb().state.remove(ProcessState::RUNNING);
    compiler_fence(Ordering::Release);

    // 发起调度,使当前内核线程休眠
    sched();

    retval = -EINTR;

    // 如果发起者没有调用kthread_stop,则运行线程函数
    if !self_kthread.flags.contains(KTHREAD_SHOULD_STOP) {
        retval = (thread_fn)(data);
    }

    kthread_exit(retval)
}

pub fn kthread_exit(result: i32) {
    let mut kt = to_kthread(&current_pcb()).clone();
    kt.result = result;
    kt.exited = true;
    unsafe{
        process_do_exit(0);
    }
}

pub fn kthread_should_stop(kthread_info: &Mutex<KThreadInfo>) -> bool {
    let kthread_info = kthread_info.lock().unwrap();
    if kthread_info.flags & (1 << 0) != 0 {
        return true;
    }
    return false;
}

/* 
 * @brief 初始化kthread机制(只应被process_init调用)
 *
 * @return isize 错误码
 */
pub fn kthread_mechanism_init() -> isize {
    kinfo!("Initializing kthread mechanism...");

    // 创建kthreadd守护进程
    unsafe{
        kernel_thread(kthreadd, null_mut(), CLONE_FS | CLONE_SIGNAL);
    }
    return 0;
}


/**
 * @brief 向kthread发送停止信号，请求其结束
 *
 * @param pcb 内核线程的pcb
 * @return isize 错误码
 */
pub fn kthread_stop(pcb: &Arc<ProcessControlBlock>) -> i32 {
    let mut retval: i32 = 0;
    let target: & KThreadInfo = to_kthread(pcb);
    target.flags |= 1 << KThreadBits::ShouldStop as i32;
    process_wakeup(pcb);
    // 等待指定的内核线程退出
    // todo: 使用completion机制改进这里
    while target.exited == false {
        rs_usleep(5000);
    }
    retval = target.result;

    // 释放内核线程的页表
    process_exit_mm(pcb);
    process_release_pcb(pcb);
    return retval;
}
/**
 * @brief 设置pcb中的worker_private字段（只应被设置一次）
 *
 * @param pcb pcb
 * @return bool 成功或失败
 */
pub fn kthread_set_worker_private(pcb: &Arc<ProcessControlBlock>) -> bool {

    if pcb.inner.read().worker_private.is_some() {
        return false;
    }
    let kt: *mut KThreadInfo = kzalloc(std::mem::size_of::<KThreadInfo>(), 0) as *mut KThreadInfo;
    if kt == null_mut() {
         return false;
    }
    unsafe {       
        pcb.inner.read().worker_private= kt as *mut u8;
    }    
    return true;
}
