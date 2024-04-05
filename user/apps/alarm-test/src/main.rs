extern crate libc;
use libc::{alarm, signal, SIGALRM, sleep};

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
        alarm(5);
    }
    println!("Alarm set for 5 seconds");
    unsafe {
        sleep(6);
    }
    println!("Test 1 complete");

    //test2: 在上一个alarm定时器完成时重新调用alarm，查看返回值是否为0,并test
    //第二个alarm能否正常运行
    unsafe {
        let remaining = alarm(5);
        println!("Remaining time for previous alarm: {}", remaining);
    }
    unsafe {
        sleep(6);
    }
    println!("Test 2 complete");

    //test3：在上一个alarm定时器未完成时重新调用alarm，查看返回值是否为上一个alarm的剩余秒数，
    //并test第二个alarm定时器能否正常运行
    unsafe {
        alarm(5);
    }
    unsafe {
        sleep(2);
    }
    unsafe {
        let remaining = alarm(5);
        println!("Remaining time for previous alarm: {}", remaining);
    }
    unsafe {
        sleep(6);
    }
    println!("Test 3 complete");
    println!("All test finish success!");
}