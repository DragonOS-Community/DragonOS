use libc::{tcgetattr, tcsetattr, termios, STDIN_FILENO, TCSANOW, VERASE};

fn main() {
    unsafe {
        let mut termios_p = std::mem::zeroed::<termios>();

        // 获取当前 STDIN_FILENO (0) 的 termios 配置
        if tcgetattr(STDIN_FILENO, &mut termios_p) != 0 {
            panic!("tcgetattr failed");
        }
        // println!("before change termios_p: {:?}", termios_p.c_cc);

        // 修改 erase 键为 127 (^?)
        termios_p.c_cc[9] = 127;

        // println!("after change termios_p: {:?}", termios_p.c_cc);

        // 立刻生效
        if tcsetattr(STDIN_FILENO, TCSANOW, &termios_p) != 0 {
            panic!("tcsetattr failed");
        }
    }
}
