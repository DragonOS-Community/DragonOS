use errno::errno;
use libc::{c_void, close, open, read, write, O_DIRECTORY, O_RDONLY, O_WRONLY};

fn report(case: &str, ok: bool, detail: &str) -> bool {
    if ok {
        println!("[PASS] {}: {}", case, detail);
    } else {
        println!("[FAIL] {}: {}", case, detail);
    }
    ok
}

fn main() {
    let mut all_pass = true;
    let path = b"/dev/pts\0";

    let fd = unsafe { open(path.as_ptr() as *const i8, O_RDONLY | O_DIRECTORY) };
    let open_rd_ok = fd >= 0;
    all_pass &= report(
        "open /dev/pts O_RDONLY|O_DIRECTORY",
        open_rd_ok,
        if open_rd_ok {
            "open succeeded"
        } else {
            "open failed unexpectedly"
        },
    );

    if fd >= 0 {
        let mut buf = [0u8; 16];

        let ret_read = unsafe { read(fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
        let err_read = errno().0;
        let read_ok = ret_read == -1 && err_read == libc::EISDIR;
        all_pass &= report(
            "read() on /dev/pts dir fd",
            read_ok,
            &format!("ret={}, errno={}", ret_read, err_read),
        );

        let ret_write = unsafe { write(fd, buf.as_ptr() as *const c_void, 1) };
        let err_write = errno().0;
        let write_ok = ret_write == -1 && (err_write == libc::EISDIR || err_write == libc::EBADF);
        all_pass &= report(
            "write() on /dev/pts dir fd",
            write_ok,
            &format!("ret={}, errno={}", ret_write, err_write),
        );

        let ret_close = unsafe { close(fd) };
        let close_ok = ret_close == 0;
        all_pass &= report(
            "close /dev/pts fd",
            close_ok,
            if close_ok {
                "close succeeded"
            } else {
                "close failed"
            },
        );
    }

    let fd_wr = unsafe { open(path.as_ptr() as *const i8, O_WRONLY) };
    let err_wr_open = errno().0;
    let open_wr_ok = fd_wr == -1 && err_wr_open == libc::EISDIR;
    all_pass &= report(
        "open /dev/pts O_WRONLY",
        open_wr_ok,
        &format!("ret={}, errno={}", fd_wr, err_wr_open),
    );
    if fd_wr >= 0 {
        unsafe { close(fd_wr) };
    }

    if all_pass {
        println!("All devpts EISDIR tests passed");
        std::process::exit(0);
    }

    println!("devpts EISDIR tests failed");
    std::process::exit(1);
}
