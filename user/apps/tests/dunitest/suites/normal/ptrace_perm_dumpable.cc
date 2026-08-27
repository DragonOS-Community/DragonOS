// ptrace 权限与 dumpability 联动回归测试。
//
// 覆盖语义：
// 1. tracee 经 setgid+setuid 降权后 dumpable 归零，同 uid 且无能力的
//    tracer ATTACH 必须被拒绝（EPERM）；
// 2. tracee prctl(PR_SET_DUMPABLE, 1) 后同一 tracer ATTACH 成功，
//    停止事件正常上报，DETACH 干净退出；
// 3. dumpability 不影响 TRACEME（自愿跟踪只走 capability 判定）。
//
// dunitest 以 root 运行：降权只发生在 fork 出的子进程内，父进程保持
// root 负责清理。

#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include <gtest/gtest.h>

// ptrace 请求号（编译环境头文件缺失时兜底，取值与 Linux x86_64 ABI 一致）
#ifndef PTRACE_TRACEME
#define PTRACE_TRACEME 0
#endif
#ifndef PTRACE_ATTACH
#define PTRACE_ATTACH 16
#endif
#ifndef PTRACE_DETACH
#define PTRACE_DETACH 17
#endif
#ifndef PR_SET_DUMPABLE
#define PR_SET_DUMPABLE 4
#endif

namespace {

constexpr int kTestUid = 1000;

long ptrace_call(long request, long pid, unsigned long addr, unsigned long data) {
    return syscall(SYS_ptrace, request, pid, addr, data);
}

// 降权到 kTestUid：gid 与 uid 一并降，保证与 tracee 的 gid 六元组匹配。
// 成功返回 0，失败返回 errno。
int drop_to_test_ids() {
    if (setgid(kTestUid) != 0) {
        return errno;
    }
    if (setuid(kTestUid) != 0) {
        return errno;
    }
    return 0;
}

bool write_byte(int fd, char value) {
    return write(fd, &value, 1) == 1;
}

bool read_byte(int fd, char* out) {
    return read(fd, out, 1) == 1;
}

struct DumpableThreadSync {
    pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
    pthread_cond_t cond = PTHREAD_COND_INITIALIZER;
    int phase = 0;
    int observed = -1;
};

void* dumpable_thread(void* opaque) {
    auto* sync = static_cast<DumpableThreadSync*>(opaque);
    pthread_mutex_lock(&sync->lock);
    while (sync->phase == 0) {
        pthread_cond_wait(&sync->cond, &sync->lock);
    }
    pthread_mutex_unlock(&sync->lock);

    sync->observed = prctl(PR_GET_DUMPABLE, 0, 0, 0, 0);
    if (sync->observed == 0) {
        prctl(PR_SET_DUMPABLE, 1, 0, 0, 0);
    }

    pthread_mutex_lock(&sync->lock);
    sync->phase = 2;
    pthread_cond_signal(&sync->cond);
    pthread_mutex_unlock(&sync->lock);
    return nullptr;
}

}  // namespace

TEST(PtracePermDumpable, DumpabilityIsSharedByThreadsInOneMm) {
    ASSERT_EQ(0, prctl(PR_SET_DUMPABLE, 0, 0, 0, 0));

    DumpableThreadSync sync;
    pthread_t thread;
    ASSERT_EQ(0, pthread_create(&thread, nullptr, dumpable_thread, &sync));

    pthread_mutex_lock(&sync.lock);
    sync.phase = 1;
    pthread_cond_signal(&sync.cond);
    while (sync.phase != 2) {
        pthread_cond_wait(&sync.cond, &sync.lock);
    }
    pthread_mutex_unlock(&sync.lock);

    ASSERT_EQ(0, pthread_join(thread, nullptr));
    EXPECT_EQ(0, sync.observed);
    EXPECT_EQ(1, prctl(PR_GET_DUMPABLE, 0, 0, 0, 0));
}

TEST(PtracePermDumpable, DumpabilityGatesAttachButNotTraceme) {
    // ---- 场景 C：dumpable=0 不影响 TRACEME ----
    // 父进程是 root（持 CAP_SYS_PTRACE），降权子进程 TRACEME 必须成功。
    {
        pid_t child = fork();
        ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";
        if (child == 0) {
            if (geteuid() != 0) {
                // 非 root 环境无法构造降权场景，跳过（沿用 capability.cc 先例）。
                _exit(0);
            }
            if (drop_to_test_ids() != 0) {
                _exit(60);
            }
            // 此时 dumpable 已经因 setuid 降权归零。
            if (ptrace_call(PTRACE_TRACEME, 0, 0, 0) != 0) {
                _exit(61);
            }
            _exit(0);
        }
        int status = 0;
        ASSERT_EQ(child, waitpid(child, &status, 0));
        ASSERT_TRUE(WIFEXITED(status)) << "child status=" << status;
        EXPECT_EQ(0, WEXITSTATUS(status)) << "TRACEME 在 dumpable=0 下应成功";
    }

    // ---- 场景 A/B：ATTACH 先被 dumpable=0 拒绝，PR_SET_DUMPABLE(1) 后成功 ----
    // 结构：父进程 fork tracer；tracer fork tracee（tracee 是 tracer 的子进程，
    // tracer 才能 waitpid 其停止事件）。
    int tracee_to_tracer[2];
    int tracer_to_tracee[2];
    ASSERT_EQ(0, pipe(tracee_to_tracer));
    ASSERT_EQ(0, pipe(tracer_to_tracee));

    pid_t tracer = fork();
    ASSERT_GE(tracer, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";
    if (tracer == 0) {
        if (geteuid() != 0) {
            // 非 root 环境下 setuid(1000) 不构成降权（dumpable 不会归零），
            // 场景不可构造，跳过（沿用 capability.cc 先例）。
            _exit(0);
        }
        // 注意：先 fork 再各自关闭不用的端——若在 fork 前关闭，
        pid_t tracee = fork();
        if (tracee < 0) {
            _exit(90);
        }
        if (tracee == 0) {
            // tracee 用 tracee_to_tracer[1] 写、tracer_to_tracee[0] 读。
            close(tracee_to_tracer[0]);
            close(tracer_to_tracee[1]);
            // tracee：降权（dumpable→0），通知后等待"置 dumpable=1"指令。
            if (drop_to_test_ids() != 0) {
                _exit(40);
            }
            if (!write_byte(tracee_to_tracer[1], 'r')) {
                _exit(41);
            }
            char cmd = 0;
            if (!read_byte(tracer_to_tracee[0], &cmd) || cmd != 'g') {
                _exit(42);
            }
            if (prctl(PR_SET_DUMPABLE, 1, 0, 0, 0) != 0) {
                _exit(43);
            }
            if (!write_byte(tracee_to_tracer[1], 'd')) {
                _exit(44);
            }
            // 等待被 attach 停止或被清理，不主动退出。
            for (;;) {
                pause();
            }
        }

        // tracer 用 tracee_to_tracer[0] 读、tracer_to_tracee[1] 写。
        close(tracee_to_tracer[1]);
        close(tracer_to_tracee[0]);


        // tracer：降权到与 tracee 相同身份。
        int err = drop_to_test_ids();
        if (err != 0) {
            kill(tracee, SIGKILL);
            _exit(50 + (err > 0 && err < 10 ? err : 9));
        }

        char c = 0;
        // tracee 已降权且 dumpable=0。
        if (!read_byte(tracee_to_tracer[0], &c)) {
            kill(tracee, SIGKILL);
            _exit(20);
        }

        errno = 0;
        if (ptrace_call(PTRACE_ATTACH, tracee, 0, 0) != -1) {
            kill(tracee, SIGKILL);
            _exit(21);  // dumpable=0 时不应放行
        }
        if (errno != EPERM) {
            kill(tracee, SIGKILL);
            _exit(22);  // 期望 EPERM
        }

        // 通知 tracee 置 dumpable=1，再次 attach。
        if (!write_byte(tracer_to_tracee[1], 'g')) {
            kill(tracee, SIGKILL);
            _exit(23);
        }
        if (!read_byte(tracee_to_tracer[0], &c)) {
            kill(tracee, SIGKILL);
            _exit(24);
        }

        if (ptrace_call(PTRACE_ATTACH, tracee, 0, 0) != 0) {
            kill(tracee, SIGKILL);
            _exit(25);  // dumpable=1 后应成功
        }
        int status = 0;
        if (waitpid(tracee, &status, WUNTRACED) != tracee || !WIFSTOPPED(status)) {
            kill(tracee, SIGKILL);
            _exit(26);  // attach 后应观察到停止
        }
        if (ptrace_call(PTRACE_DETACH, tracee, 0, 0) != 0) {
            kill(tracee, SIGKILL);
            _exit(27);
        }
        if (kill(tracee, SIGKILL) != 0) {
            _exit(28);
        }
        if (waitpid(tracee, &status, 0) != tracee || !WIFSIGNALED(status) ||
            WTERMSIG(status) != SIGKILL) {
            _exit(29);
        }
        _exit(0);
    }

    close(tracee_to_tracer[0]);
    close(tracee_to_tracer[1]);
    close(tracer_to_tracee[0]);
    close(tracer_to_tracee[1]);

    int status = 0;
    ASSERT_EQ(tracer, waitpid(tracer, &status, 0));
    ASSERT_TRUE(WIFEXITED(status)) << "tracer status=" << status;
    const int code = WEXITSTATUS(status);
    EXPECT_EQ(0, code) << "tracer 退出码含义：20/24=管道同步失败 21=dumpable=0 时 ATTACH 未拒绝"
                          " 22=errno 非 EPERM 25=dumpable=1 后 ATTACH 失败 26=未观察到停止"
                          " 27=DETACH 失败 28/29=清理失败 4x=tracee 准备失败 5x=tracer 降权失败"
                          " 60/61=TRACEME 场景失败 90=fork 失败";
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
