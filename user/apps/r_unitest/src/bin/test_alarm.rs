extern crate libc;
use libc::{signal, sleep, syscall, SYS_alarm, SIGALRM};

extern "C" fn handle_alarm(_: i32) {
    println!("Alarm ring!");
}

fn main() {
    // 设置信号处理函数
    unsafe {
        signal(SIGALRM, handle_alarm as usize);
    }

    //test1: alarm系统调用能否正常运行
    unsafe {
        syscall(SYS_alarm, 5);
    }
    println!("Alarm set for 5 seconds");
    unsafe {
        sleep(6);
    }
    println!("Test 1 complete");

    //test2：在上一个alarm定时器未完成时重新调用alarm，查看返回值是否为上一个alarm的剩余秒数，
    //并test第三个alarm定时器能否正常运行

    unsafe {
        let remaining = syscall(SYS_alarm, 5);
        println!("Remaining time for previous alarm: {}", remaining);
    }
    println!("Alarm set for 5 seconds");
    unsafe {
        let remaining = syscall(SYS_alarm, 3);
        println!("Remaining time for previous alarm: {}", remaining);
    }
    unsafe {
        sleep(4);
    }
    println!("Alarm set for 3 seconds");

    println!("Test 2 complete");
}
