#include "strace_format.h"

#include <iostream>
#include <sys/ptrace.h>
#include <sys/wait.h>
#include <unistd.h>

using namespace std;

// 追踪子进程
int trace_child(pid_t child_pid) {
    int status;
    bool first_stop = true;
    int last_syscall = -1;

    while (true) {
        // 等待子进程状态变化
        if (waitpid(child_pid, &status, 0) == -1) {
            cerr << "waitpid error: " << strerror(errno) << '\n';
            return 1;
        }
        // 检查子进程是否退出
        if (WIFEXITED(status)) {
            cout << "\n+++ exited with " << WEXITSTATUS(status) << " +++" << '\n';
            return 0;
        }
        // 检查是否收到信号
        if (WIFSIGNALED(status)) {
            cout << "\n+++ killed by " << strsignal(WTERMSIG(status)) << " +++" << '\n';
            return 0;
        }
        // 首次跟踪需要忽略SIGTRAP
        if (first_stop && WIFSTOPPED(status) && WSTOPSIG(status) == SIGTRAP) {
            first_stop = false;
            ptrace(PTRACE_SETOPTIONS, child_pid, 0, PTRACE_O_TRACESYSGOOD);
            if (ptrace(PTRACE_SYSCALL, child_pid, 0, 0) == -1) {
                cerr << "ptrace(SYSCALL) failed: " << strerror(errno) << '\n';
                return 1;
            }
            continue;
        }

        // 处理系统调用入口事件
        if (WIFSTOPPED(status) && WSTOPSIG(status) == (SIGTRAP | 0x80)) {
            user_regs_struct regs;
            if (ptrace(PTRACE_GETREGS, child_pid, 0, &regs) == -1) {
                cerr << "ptrace(GETREGS) failed: " << strerror(errno) << '\n';
                return 1;
            }
            if (last_syscall == -1) {
                // 获取系统调用号
                int syscall_num = static_cast<int>(SYSCALL_REG(regs));
                last_syscall = syscall_num;
                cout << format_arguments(
                    child_pid, syscall_num, ARG1(regs), ARG2(regs), ARG3(regs), ARG4(regs), ARG5(regs), ARG6(regs));
            } else {
                long return_value = RETURN_REG(regs);
                cout << format_return_value(return_value) << '\n';
                last_syscall = -1;
            }
            // 继续执行等待系统调用完成
            if (ptrace(PTRACE_SYSCALL, child_pid, 0, 0) == -1) {
                cerr << "ptrace(SYSCALL) failed: " << strerror(errno) << '\n';
                return 1;
            }
        } else if (WIFSTOPPED(status)) { // 处理其他事件
            // 继续执行子进程
            if (ptrace(PTRACE_SYSCALL, child_pid, 0, WSTOPSIG(status)) == -1) {
                cerr << "ptrace(SYSCALL) failed: " << strerror(errno) << '\n';
                return 1;
            }
        }
    }
    return 0;
}

int main(int argc, char* argv[]) {
    if (argc < 2) {
        cerr << "Usage: " << argv[0] << " must have PROG [ARGS] or -p PID" << '\n';
        return 1;
    }

    pid_t pid = fork();
    if (pid == -1) {
        cerr << "fork failed: " << strerror(errno) << '\n';
        return 1;
    }
    // 子进程执行目标程序
    if (pid == 0) {
        // 启用跟踪
        ptrace(PTRACE_TRACEME, 0, 0, 0);
        // 设置execve参数
        vector<char*> args;
        for (int i = 1; i < argc; ++i) {
            args.push_back(argv[i]);
        }
        args.push_back(nullptr);
        // 执行目标程序
        execvp(args[0], args.data());
        // 若exec失败
        cerr << "execvp failed: " << strerror(errno) << '\n';
        return 1;
    } else { // 父进程执行跟踪
        return trace_child(pid);
    }
}