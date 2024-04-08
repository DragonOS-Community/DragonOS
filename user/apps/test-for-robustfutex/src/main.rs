extern crate libc;
extern crate syscalls;

use std::{
    ffi::c_void,
    mem::{self, size_of},
    process,
    ptr::{self, NonNull},
    sync::atomic::{AtomicI32, Ordering},
    thread,
    time::Duration,
};

use syscalls::{
    syscall0, syscall2, syscall3, syscall6,
    Sysno::{futex, get_robust_list, gettid, set_robust_list},
};

use libc::{
    c_int, mmap, perror, EXIT_FAILURE, MAP_ANONYMOUS, MAP_FAILED, MAP_SHARED, PROT_READ, PROT_WRITE,
};

const FUTEX_WAIT: usize = 0;
const FUTEX_WAKE: usize = 1;

// 封装futex
#[derive(Clone, Copy, Debug)]
struct Futex {
    addr: *mut u32,
}

impl Futex {
    pub fn new(addr: *mut u32) -> Self {
        return Futex { addr };
    }

    pub fn get_addr(&self, offset: isize) -> *mut u32 {
        return unsafe { self.addr.offset(offset) };
    }

    pub fn get_val(&self, offset: isize) -> u32 {
        return unsafe { self.addr.offset(offset).read() };
    }

    pub fn set_val(&self, val: u32, offset: isize) {
        unsafe {
            self.addr.offset(offset).write(val);
        }
    }
}

unsafe impl Send for Futex {}
unsafe impl Sync for Futex {}

#[derive(Clone, Copy, Debug)]
struct Lock {
    addr: *mut i32,
}

impl Lock {
    pub fn new(addr: *mut i32) -> Self {
        return Lock { addr };
    }

    pub fn get_val(&self, offset: isize) -> i32 {
        return unsafe { self.addr.offset(offset).read() };
    }

    pub fn set_val(&self, val: i32, offset: isize) {
        unsafe {
            self.addr.offset(offset).write(val);
        }
    }
}

unsafe impl Send for Lock {}
unsafe impl Sync for Lock {}

#[derive(Debug, Clone, Copy)]
struct RobustList {
    next: *const RobustList,
}

#[derive(Debug, Clone, Copy)]
struct RobustListHead {
    list: RobustList,
    /// 向kernel提供了要检查的futex字段的相对位置，保持用户空间的灵活性，可以自由
    /// 地调整其数据结构，而无需向内核硬编码任何特定的偏移量
    /// futexes中前面的地址是用来存入robust list中(list.next)，后面是存放具体的futex val
    /// 这个字段的作用就是从前面的地址偏移到后面的地址中从而获取futex val
    #[allow(dead_code)]
    futex_offset: isize,
    /// 潜在的竞争条件：由于添加和删除列表是在获取锁之后进行的，这給线程留下了一个小窗口，在此期间可能会导致异常退出，
    /// 从而使锁被悬挂，为了防止这种可能性。用户空间还维护了一个简单的list_op_pending字段，允许线程在获取锁后但还未添加到
    /// 列表时就异常退出时进行清理。并且在完成列表添加或删除操作后将其清除
    /// 这里没有测试这个，在内核中实现实际上就是把list_op_pending地址进行一次唤醒（如果有等待者）
    #[allow(dead_code)]
    list_op_pending: *const RobustList,
}

fn error_handle(msg: &str) -> ! {
    unsafe { perror(msg.as_ptr() as *const i8) };
    process::exit(EXIT_FAILURE)
}

fn futex_wait(futexes: Futex, thread: &str, offset_futex: isize, lock: Lock, offset_count: isize) {
    loop {
        let atomic_count = AtomicI32::new(lock.get_val(offset_count));
        if atomic_count
            .compare_exchange(1, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            lock.set_val(0, offset_count);

            // 设置futex锁当前被哪个线程占用
            let tid = unsafe { syscall0(gettid).unwrap() as u32 };
            futexes.set_val(futexes.get_val(offset_futex) | tid, offset_futex);

            break;
        }

        println!("{} wating...", thread);
        let futex_val = futexes.get_val(offset_futex);
        futexes.set_val(futex_val | 0x8000_0000, offset_futex);
        let ret = unsafe {
            syscall6(
                futex,
                futexes.get_addr(offset_futex) as usize,
                FUTEX_WAIT,
                futexes.get_val(offset_futex) as usize,
                0,
                0,
                0,
            )
        };
        if ret.is_err() {
            error_handle("futex_wait failed");
        }

        // 被唤醒后释放锁
        let atomic_count = AtomicI32::new(lock.get_val(offset_count));
        if atomic_count
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            lock.set_val(1, offset_count);

            // 释放futex锁，不被任何线程占用
            futexes.set_val(futexes.get_val(offset_futex) & 0xc000_0000, offset_futex);

            break;
        }
    }
}

fn futex_wake(futexes: Futex, thread: &str, offset_futex: isize, lock: Lock, offset_count: isize) {
    let atomic_count = AtomicI32::new(lock.get_val(offset_count));
    if atomic_count
        .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        lock.set_val(1, offset_count);

        // 释放futex锁，不被任何线程占用
        futexes.set_val(futexes.get_val(offset_futex) & 0xc000_0000, offset_futex);

        // 如果没有线程/进程在等这个futex，则不必唤醒, 释放改锁即可
        let futex_val = futexes.get_val(offset_futex);
        if futex_val & 0x8000_0000 == 0 {
            return;
        }

        futexes.set_val(futex_val & !(1 << 31), offset_futex);
        let ret = unsafe {
            syscall6(
                futex,
                futexes.get_addr(offset_futex) as usize,
                FUTEX_WAKE,
                1,
                0,
                0,
                0,
            )
        };
        if ret.is_err() {
            error_handle("futex wake failed");
        }
        println!("{} waked", thread);
    }
}

fn set_list(futexes: Futex) {
    let head = RobustListHead {
        list: RobustList { next: ptr::null() },
        futex_offset: 44,
        list_op_pending: ptr::null(),
    };
    let head = NonNull::from(&head).as_ptr();
    unsafe {
        // 加入第一个futex
        let head_ref_mut = &mut *head;
        head_ref_mut.list.next = futexes.get_addr(0) as *const RobustList;

        // 加入第二个futex
        let list_2 = NonNull::from(&*head_ref_mut.list.next).as_ptr();
        let list_2_ref_mut = &mut *list_2;
        list_2_ref_mut.next = futexes.get_addr(1) as *const RobustList;

        //println!("robust list next: {:?}", (*head).list.next );
        //println!("robust list next next: {:?}", (*(*head).list.next).next );

        // 向内核注册robust list
        let len = mem::size_of::<*mut RobustListHead>();
        let ret = syscall2(set_robust_list, head as usize, len);
        if ret.is_err() {
            println!("failed to set_robust_list, ret = {:?}", ret);
        }
    }
}

fn main() {
    test01();

    println!("-------------");

    test02();

    println!("-------------");
}

//测试set_robust_list和get_robust_list两个系统调用是否能正常使用
fn test01() {
    // 创建robust list 头指针
    let head = RobustListHead {
        list: RobustList { next: ptr::null() },
        futex_offset: 8,
        list_op_pending: ptr::null(),
    };
    let head = NonNull::from(&head).as_ptr();

    let futexes = unsafe {
        mmap(
            ptr::null_mut::<c_void>(),
            (size_of::<c_int>() * 2) as libc::size_t,
            PROT_READ | PROT_WRITE,
            MAP_ANONYMOUS | MAP_SHARED,
            -1,
            0,
        ) as *mut u32
    };
    if futexes == MAP_FAILED as *mut u32 {
        error_handle("futexes_addr mmap failed");
    }

    unsafe {
        futexes.offset(11).write(0x0000_0000);
        futexes.offset(12).write(0x8000_0000);
        println!("futex1 next addr: {:?}", futexes.offset(0));
        println!("futex2 next addr: {:?}", futexes.offset(1));
        println!("futex1 val addr: {:?}", futexes.offset(11));
        println!("futex2 val addr: {:?}", futexes.offset(12));
        println!("futex1 val: {:#x?}", futexes.offset(11).read());
        println!("futex2 val: {:#x?}", futexes.offset(12).read());
    }

    // 打印注册之前的robust list
    println!("robust list next(get behind): {:?}", &unsafe { *head });

    unsafe {
        let head_ref_mut = &mut *head;
        head_ref_mut.list.next = futexes.offset(0) as *const RobustList;
        let list_2 = NonNull::from(&*head_ref_mut.list.next).as_ptr();
        let list_2_ref_mut = &mut *list_2;
        list_2_ref_mut.next = futexes.offset(1) as *const RobustList;
        println!("robust list next addr: {:?}", (*head).list.next);
        println!(
            "robust list next next addr: {:?}",
            (*(*head).list.next).next
        );
    }

    unsafe {
        let len = mem::size_of::<*mut RobustListHead>();
        let ret = syscall2(set_robust_list, head as usize, len);
        if ret.is_err() {
            println!("failed to set_robust_list, ret = {:?}", ret);
        }
    }

    println!("get before, set after: {:?}", head);
    println!("get before, set after: {:?}", &unsafe { *head });
    unsafe {
        let len: usize = 0;
        println!("len = {}", len);
        let len_ptr = NonNull::from(&len).as_ptr();
        let ret = syscall3(get_robust_list, 0, head as usize, len_ptr as usize);
        println!("get len = {}", len);
        if ret.is_err() {
            println!("failed to get_robust_list, ret = {:?}", ret);
        }

        println!("futex1 val: {:#x}", futexes.offset(11).read());
        println!("futex2 val: {:#x}", futexes.offset(12).read());
        println!("robust list next: {:?}", futexes.offset(0));
        println!("robust list next next: {:#x?}", futexes.offset(0).read());
    }
    println!("robust list head(get after): {:?}", head);
    println!("robust list next(get after): {:?}", &unsafe { *head });
}

//测试一个线程异常退出时futex的robustness(多线程测试，目前futex还不支持多进程)
fn test02() {
    let futexes = unsafe {
        mmap(
            ptr::null_mut::<c_void>(),
            (size_of::<c_int>() * 2) as libc::size_t,
            PROT_READ | PROT_WRITE,
            MAP_ANONYMOUS | MAP_SHARED,
            -1,
            0,
        ) as *mut u32
    };
    if futexes == MAP_FAILED as *mut u32 {
        error_handle("mmap failed");
    }
    let count = unsafe {
        mmap(
            ptr::null_mut::<c_void>(),
            (size_of::<c_int>() * 2) as libc::size_t,
            PROT_READ | PROT_WRITE,
            MAP_ANONYMOUS | MAP_SHARED,
            -1,
            0,
        ) as *mut i32
    };
    if count == MAP_FAILED as *mut i32 {
        error_handle("mmap failed");
    }

    unsafe {
        // 在这个示例中，第一段和第二段地址放入robust list，第11段地址和第12段地址存放futex val
        futexes.offset(11).write(0x0000_0000);
        futexes.offset(12).write(0x0000_0000);
        println!("futex1 next addr: {:?}", futexes.offset(0));
        println!("futex2 next addr: {:?}", futexes.offset(1));
        println!("futex1 val addr: {:?}", futexes.offset(11));
        println!("futex2 val addr: {:?}", futexes.offset(12));
        println!("futex1 val: {:#x?}", futexes.offset(11).read());
        println!("futex2 val: {:#x?}", futexes.offset(12).read());

        count.offset(0).write(1);
        count.offset(1).write(0);
        println!("count1 val: {:?}", count.offset(0).read());
        println!("count2 val: {:?}", count.offset(1).read());
    }

    let futexes = Futex::new(futexes);
    let locks = Lock::new(count);

    // tid1 = 7
    let thread1 = thread::spawn(move || {
        set_list(futexes);
        thread::sleep(Duration::from_secs(2));
        for i in 0..2 {
            futex_wait(futexes, "thread1", 11, locks, 0);
            println!("thread1 times: {}", i);
            thread::sleep(Duration::from_secs(3));

            let tid = unsafe { syscall0(gettid).unwrap() as u32 };
            futexes.set_val(futexes.get_val(12) | tid, 12);

            if i == 1 {
                // 让thread1异常退出，从而无法唤醒thread2,检测robustness
                println!("Thread1 exiting early due to simulated error.");
                return;
            }
            futex_wake(futexes, "thread2", 12, locks, 1);
        }
    });

    // tid2 = 6
    set_list(futexes);
    for i in 0..2 {
        futex_wait(futexes, "thread2", 12, locks, 1);
        println!("thread2 times: {}", i);

        let tid = unsafe { syscall0(gettid).unwrap() as u32 };
        futexes.set_val(futexes.get_val(11) | tid, 11);

        futex_wake(futexes, "thread1", 11, locks, 0);
    }

    thread1.join().unwrap();
}
