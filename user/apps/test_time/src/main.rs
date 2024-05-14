use std::time::{Instant, Duration};

fn main() {
    let mut cnt = 10;
    loop {
        if cnt <= 0 {
            break;
        }
        
        let now_time = Instant::now();
        let next_time = Instant::now();

        if next_time + Duration::from_secs(10) < now_time { // 误差达到10秒以上
            println!("Now Time: {:?} > Next Time: {:?}", now_time, next_time);
            cnt -= 1;
        }

        // 设置时间间隔为 20 毫秒
        //std::thread::sleep(Duration::from_millis(20));
    }
}