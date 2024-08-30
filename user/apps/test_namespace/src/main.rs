extern crate nix;
use nix::sched::{self, CloneFlags};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{self, execvp, fork, ForkResult};
use std::ffi::CString;
use std::process;

fn main() {
    // 定义新的 PID 和 MNT namespaces
    let clone_flags = CloneFlags::CLONE_NEWPID | CloneFlags::CLONE_NEWNS;
    unsafe {
        match fork() {
            Ok(ForkResult::Parent { child }) => {
                // 父进程等待子进程
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
                // 子进程：在新 namespace 中执行
                println!("Child process. PID: {}", unistd::getpid());

                // 使用 `unshare` 创建新的 namespaces
                sched::unshare(clone_flags).expect("Failed to unshare");

                // 执行命令或脚本来检查 namespace 的隔离效果
                let cmd = CString::new("/bin/bash").expect("CString::new failed");
                let args = [
                    CString::new("-c").expect("CString::new failed"),
                    CString::new("echo 'Running in new PID namespace'; sleep 5; /bin/bash")
                        .expect("CString::new failed"),
                ];
                execvp(&cmd, &args).expect("Failed to execvp");
            }
            Err(err) => {
                println!("Fork failed: {:?}", err);
                process::exit(1);
            }
        }
    }
}
