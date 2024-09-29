extern crate nix;
use nix::sched::{self, CloneFlags};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{self, execvp, fork, ForkResult};
use std::ffi::CString;
use std::process;

fn main() {
    let clone_flags = CloneFlags::CLONE_NEWPID | CloneFlags::CLONE_NEWNS;

    println!("Parent process. PID: {}", unistd::getpid());
    unsafe {
        match fork() {
            Ok(ForkResult::Parent { child }) => {
                println!("Parent process. Child PID: {}", child);
                match waitpid(child, None) {
                    Ok(WaitStatus::Exited(pid, status)) => {
                        println!("Child {} exited with status: {}", pid, status);
                    }
                    Ok(_) => println!("Child process did not exit normally."),
                    Err(e) => println!("Error waiting for child process: {:?}", e),
                }
            }
            Ok(ForkResult::Child) => {
                // 使用 unshare 创建新的命名空间
                if let Err(e) = sched::unshare(clone_flags) {
                    println!("Failed to unshare: {:?}", e);
                    process::exit(1);
                }
                println!("Child process. PID: {}", unistd::getpid());
            }
            Err(err) => {
                println!("Fork failed: {:?}", err);
                process::exit(1);
            }
        }
    }
}
