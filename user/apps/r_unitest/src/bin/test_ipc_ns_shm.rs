extern crate nix;

use nix::errno::Errno;
use nix::sched::{self, CloneFlags};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{fork, getpid, pipe, read, write, ForkResult};

const IPC_CREAT: i32 = 0o1000;

fn shmget(key: i32, size: usize, flags: i32) -> Result<i32, i32> {
    let r = unsafe { libc::shmget(key as libc::key_t, size, flags as libc::c_int) };
    if r < 0 {
        return Err(Errno::last_raw());
    }
    Ok(r)
}

fn shmctl(id: i32, cmd: i32) -> Result<i32, i32> {
    let r = unsafe { libc::shmctl(id, cmd, std::ptr::null_mut()) };
    if r < 0 {
        return Err(Errno::last_raw());
    }
    Ok(r)
}

fn shmat(id: i32) -> Result<*mut libc::c_void, i32> {
    let r = unsafe { libc::shmat(id, std::ptr::null(), 0) };
    if (r as isize) == -1 {
        return Err(Errno::last_raw());
    }
    Ok(r)
}

fn shmdt(addr: *mut libc::c_void) -> Result<i32, i32> {
    let r = unsafe { libc::shmdt(addr) };
    if r < 0 {
        return Err(Errno::last_raw());
    }
    Ok(r)
}

struct Suite { passed: usize, failed: usize }

impl Suite {
    fn new() -> Self { Self { passed: 0, failed: 0 } }
    fn ok(&mut self, name: &str) { self.passed += 1; println!("[PASS] {}", name); }
    fn fail(&mut self, name: &str, msg: &str) { self.failed += 1; println!("[FAIL] {}: {}", name, msg); }
    fn finish(&self) -> i32 {
        println!("Summary: passed={}, failed={}", self.passed, self.failed);
        if self.failed == 0 { 0 } else { 1 }
    }
}

fn main() {
    let key_parent: i32 = 0x12345; // 仅父命名空间使用
    let key_child: i32 = 0x23456;  // 仅子命名空间使用
    let size: usize = 4096;
    let mut suite = Suite::new();

    println!("[parent:{}] create shm in parent ns", getpid());
    let id_parent = shmget(key_parent, size, IPC_CREAT | 0o600).expect("parent shmget failed");

    let (pr, pw) = pipe().expect("pipe");
    unsafe {
        match fork() {
            Ok(ForkResult::Parent { child }) => {
                // case2: ensure child ns's shm is not visible to parent while child alive
                // wait child signals via pipe
                let mut buf = [0u8; 1];
                use std::os::fd::AsRawFd;
                let _ = read(pr.as_raw_fd(), &mut buf).expect("read pipe");
                match shmget(key_child, size, 0o600) {
                    Ok(_) => suite.fail("parent cannot see child's shm by key_child", "unexpected shm visible"),
                    Err(e) => {
                        if e == libc::ENOENT { suite.ok("parent cannot see child's shm by key_child"); } else { suite.fail("parent cannot see child's shm by key_child", &format!("errno={}", e)); }
                    }
                }

                match waitpid(child, None) {
                    Ok(WaitStatus::Exited(_, status)) => {
                        if status == 0 { suite.ok("child exited 0"); } else { suite.fail("child exited 0", &format!("status={}", status)); }
                    }
                    Ok(other) => suite.fail("child wait status", &format!("{:?}", other)),
                    Err(e) => suite.fail("waitpid", &format!("{:?}", e)),
                }

                // parent can still attach/detach its own segment and then remove
                match shmat(id_parent) {
                    Ok(addr) => {
                        let _ = shmdt(addr).map_err(|e| suite.fail("parent shmdt", &format!("errno={}", e)));
                        let _ = shmctl(id_parent, libc::IPC_RMID).map_err(|e| suite.fail("parent IPC_RMID", &format!("errno={}", e)));
                        suite.ok("parent attach/detach and remove");
                    }
                    Err(e) => suite.fail("parent shmat", &format!("errno={}", e)),
                }

                std::process::exit(suite.finish());
            }
            Ok(ForkResult::Child) => {
                println!("[child:{}] unshare CLONE_NEWIPC", getpid());
                if let Err(e) = sched::unshare(CloneFlags::CLONE_NEWIPC) { eprintln!("unshare failed: {:?}", e); std::process::exit(2); }

                // case1: child can't see parent's shm by key
                match shmget(key_parent, size, 0o600) {
                    Ok(_) => { println!("[FAIL] child cannot see parent's shm by key"); std::process::exit(3); }
                    Err(e) => if e == libc::ENOENT { println!("[PASS] child cannot see parent's shm by key"); } else { println!("[FAIL] child cannot see parent's shm by key: errno={}", e); std::process::exit(3); }
                }

                // create its own shm with a different key in new ipc ns
                let id_child = match shmget(key_child, size, IPC_CREAT | 0o600) { Ok(x) => x, Err(e) => { eprintln!("child shmget(key_child) failed errno={}", e); std::process::exit(4); } };
                // also create same key as parent in child ns to ensure no conflict across ns
                let _id_child_same_key = match shmget(key_parent, size, IPC_CREAT | 0o600) { Ok(x) => x, Err(e) => { eprintln!("child shmget(key_parent in child ns) failed errno={}", e); std::process::exit(4); } };
                // signal parent
                let _ = write(pw, &[1u8]).expect("write pipe");

                // shmat + write, then mark IPC_RMID and ensure we can create new shm with the same key
                let addr = match shmat(id_child) { Ok(p) => p, Err(e) => { eprintln!("child shmat failed errno={}", e); std::process::exit(5); } };
                unsafe { *(addr as *mut u8) = 0xAB; }
                // mark to be removed
                let _ = shmctl(id_child, libc::IPC_RMID).expect("IPC_RMID failed");
                // now shmget with same key and EXCL should succeed with a new id
                let id2 = shmget(key_child, size, IPC_CREAT | 0o600 | libc::IPC_EXCL).expect("child shmget recreate failed");
                if id2 == id_child { eprintln!("id2 should differ from id_child"); std::process::exit(6); }
                // detach old mapping
                let _ = shmdt(addr).expect("child shmdt failed");
                // cleanup new one
                let addr2 = shmat(id2).expect("child shmat id2 failed");
                let _ = shmdt(addr2).expect("child shmdt id2 failed");
                let _ = shmctl(id2, libc::IPC_RMID).expect("child IPC_RMID id2 failed");
                std::process::exit(0);
            }
            Err(err) => panic!("fork failed: {:?}", err),
        }
    }
}


