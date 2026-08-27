#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <errno.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <pthread.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/ptrace.h>
#include <sys/syscall.h>
#include <sys/ucontext.h>
#include <sys/user.h>
#include <sys/wait.h>
#include <time.h>
#include <ucontext.h>
#include <unistd.h>

#include <atomic>

#include <gtest/gtest.h>

#ifndef AUDIT_ARCH_X86_64
#define AUDIT_ARCH_X86_64 0xC000003E
#endif

#ifndef SECCOMP_RET_KILL_PROCESS
#define SECCOMP_RET_KILL_PROCESS 0x80000000U
#endif

#ifndef SYS_SECCOMP
#define SYS_SECCOMP 1
#endif

#ifndef PTRACE_EVENT_SECCOMP
#define PTRACE_EVENT_SECCOMP 7
#endif

namespace {

constexpr int kOk = 42;
constexpr long kTrapReturn = 424242;
constexpr int kTrapData = 0xdead;
constexpr int kDeadlineMs = 3000;
char g_trace_payload = 'Q';

long PtraceCall(long request, pid_t pid, unsigned long addr,
                unsigned long data) {
  return syscall(SYS_ptrace, request, pid, addr, data);
}

int64_t MonotonicMillis() {
  timespec now = {};
  if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) return -1;
  return static_cast<int64_t>(now.tv_sec) * 1000 + now.tv_nsec / 1000000;
}

pid_t WaitPidDeadline(pid_t pid, int* status, int options = 0,
                      int timeout_ms = kDeadlineMs) {
  const int64_t start = MonotonicMillis();
  if (start < 0) return -1;
  const int64_t deadline = start + timeout_ms;
  for (;;) {
    const pid_t result = waitpid(pid, status, options | WNOHANG);
    if (result != 0) return result;
    if (MonotonicMillis() >= deadline) {
      errno = ETIMEDOUT;
      return -1;
    }
    sched_yield();
  }
}

bool ReadByteDeadline(int fd, char* byte, int timeout_ms = kDeadlineMs) {
  const int64_t start = MonotonicMillis();
  if (start < 0) return false;
  const int64_t deadline = start + timeout_ms;
  pollfd event = {.fd = fd, .events = POLLIN, .revents = 0};
  for (;;) {
    const int64_t now = MonotonicMillis();
    if (now < 0 || now >= deadline) {
      errno = ETIMEDOUT;
      return false;
    }
    const int ready = poll(&event, 1, static_cast<int>(deadline - now));
    if (ready > 0) return read(fd, byte, 1) == 1;
    if (ready == 0) {
      errno = ETIMEDOUT;
      return false;
    }
    if (errno != EINTR) return false;
  }
}

class ScopedChild {
 public:
  explicit ScopedChild(pid_t child) : child_(child) {}
  ~ScopedChild() {
    if (child_ <= 0) return;
    kill(child_, SIGKILL);
    int status = 0;
    (void)WaitPidDeadline(child_, &status, 0, 1000);
  }
  void Release() { child_ = -1; }

 private:
  pid_t child_;
};

int InstallFilterWithFlags(const struct sock_filter* filter, unsigned short len,
                           unsigned int flags) {
  struct sock_fprog prog = {
      .len = len,
      .filter = const_cast<struct sock_filter*>(filter),
  };
  return syscall(__NR_seccomp, SECCOMP_SET_MODE_FILTER, flags, &prog);
}

int InstallFilter(const struct sock_filter* filter, unsigned short len) {
  return InstallFilterWithFlags(filter, len, 0);
}

int ChildStatus(void (*child)()) {
  pid_t pid = fork();
  if (pid == 0) {
    child();
    _exit(1);
  }
  EXPECT_GT(pid, 0);
  int status = 0;
  EXPECT_EQ(waitpid(pid, &status, 0), pid);
  return status;
}

void RequireNoNewPrivs() {
  if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0) {
    _exit(2);
  }
}

void InstallGetpidTrapFilter() {
  struct sock_filter filter[] = {
      BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
      BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_getpid, 0, 1),
      BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_TRAP | kTrapData),
      BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
  };
  if (InstallFilter(filter, sizeof(filter) / sizeof(filter[0])) != 0) {
    _exit(2);
  }
}

void SeccompTrapHandler(int signo, siginfo_t* info, void* raw_ucontext) {
  if (signo != SIGSYS || info == nullptr || raw_ucontext == nullptr) {
    _exit(10);
  }
  if (info->si_signo != SIGSYS || info->si_code != SYS_SECCOMP ||
      info->si_errno != kTrapData) {
    _exit(11);
  }
  if (info->si_call_addr == nullptr || info->si_syscall != __NR_getpid ||
      info->si_arch != AUDIT_ARCH_X86_64) {
    _exit(12);
  }

  auto* ctx = reinterpret_cast<ucontext_t*>(raw_ucontext);
  if (ctx->uc_mcontext.gregs[REG_RAX] != __NR_getpid) {
    _exit(13);
  }
  ctx->uc_mcontext.gregs[REG_RAX] = kTrapReturn;
}

void* SeccompTsyncThread(void* arg) {
  auto* ready = reinterpret_cast<std::atomic<int>*>(arg);
  while (ready->load(std::memory_order_acquire) == 0) {
    sched_yield();
  }

  errno = 0;
  long ret = syscall(__NR_getpid);
  return reinterpret_cast<void*>(
      static_cast<intptr_t>(ret == -1 && errno == ENOTNAM ? kOk : 3));
}

}  // namespace

TEST(SeccompTest, StrictModeKillsForbiddenSyscall) {
  int status = ChildStatus([] {
    if (syscall(__NR_seccomp, SECCOMP_SET_MODE_STRICT, 0, nullptr) != 0) {
      _exit(2);
    }
    syscall(__NR_getpid);
    _exit(3);
  });

  ASSERT_TRUE(WIFSIGNALED(status)) << "status=" << status;
  EXPECT_EQ(WTERMSIG(status), SIGKILL);
}

TEST(SeccompTest, ErrnoActionSkipsSyscallWithRequestedErrno) {
  int status = ChildStatus([] {
    RequireNoNewPrivs();
    struct sock_filter filter[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_getpid, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | ENOENT),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    if (InstallFilter(filter, sizeof(filter) / sizeof(filter[0])) != 0) {
      _exit(2);
    }
    errno = 0;
    long ret = syscall(__NR_getpid);
    _exit(ret == -1 && errno == ENOENT ? kOk : 3);
  });

  ASSERT_TRUE(WIFEXITED(status)) << "status=" << status;
  EXPECT_EQ(WEXITSTATUS(status), kOk);
}

TEST(SeccompTest, ArchFieldMatchesNativeAuditArch) {
  int status = ChildStatus([] {
    RequireNoNewPrivs();
    struct sock_filter filter[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, arch)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_X86_64, 1, 0),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    if (InstallFilter(filter, sizeof(filter) / sizeof(filter[0])) != 0) {
      _exit(2);
    }
    syscall(__NR_getpid);
    _exit(kOk);
  });

  ASSERT_TRUE(WIFEXITED(status)) << "status=" << status;
  EXPECT_EQ(WEXITSTATUS(status), kOk);
}

TEST(SeccompTest, KillActionCannotBeCaught) {
  int status = ChildStatus([] {
    struct sigaction sa = {};
    sa.sa_handler = [](int) {};
    sigaction(SIGSYS, &sa, nullptr);

    RequireNoNewPrivs();
    struct sock_filter filter[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_getpid, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL_THREAD),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    if (InstallFilter(filter, sizeof(filter) / sizeof(filter[0])) != 0) {
      _exit(2);
    }
    syscall(__NR_getpid);
    _exit(3);
  });

  ASSERT_TRUE(WIFSIGNALED(status)) << "status=" << status;
  EXPECT_EQ(WTERMSIG(status), SIGSYS);
}

TEST(SeccompTest, KillProcessWinsOverLaterErrnoFilter) {
  int status = ChildStatus([] {
    RequireNoNewPrivs();
    struct sock_filter kill_filter[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_getpid, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    struct sock_filter errno_filter[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_getpid, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    if (InstallFilter(kill_filter, sizeof(kill_filter) / sizeof(kill_filter[0])) != 0 ||
        InstallFilter(errno_filter, sizeof(errno_filter) / sizeof(errno_filter[0])) != 0) {
      _exit(2);
    }
    syscall(__NR_getpid);
    _exit(3);
  });

  ASSERT_TRUE(WIFSIGNALED(status)) << "status=" << status;
  EXPECT_EQ(WTERMSIG(status), SIGSYS);
}

TEST(SeccompTest, TrapDeliversSysSeccompSiginfoAndCanEmulateReturn) {
  int status = ChildStatus([] {
    struct sigaction sa = {};
    sa.sa_sigaction = SeccompTrapHandler;
    sa.sa_flags = SA_SIGINFO;
    if (sigaction(SIGSYS, &sa, nullptr) != 0) {
      _exit(2);
    }

    RequireNoNewPrivs();
    InstallGetpidTrapFilter();
    errno = 0;
    long ret = syscall(__NR_getpid);
    _exit(ret == kTrapReturn && errno == 0 ? kOk : 3);
  });

  ASSERT_TRUE(WIFEXITED(status)) << "status=" << status;
  EXPECT_EQ(WEXITSTATUS(status), kOk);
}

TEST(SeccompTest, TraceWithoutPtracerReturnsEnosys) {
  int status = ChildStatus([] {
    RequireNoNewPrivs();
    struct sock_filter filter[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_getpid, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_TRACE),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    if (InstallFilter(filter, sizeof(filter) / sizeof(filter[0])) != 0) {
      _exit(2);
    }

    errno = 0;
    long ret = syscall(__NR_getpid);
    _exit(ret == -1 && errno == ENOSYS ? kOk : 3);
  });

  ASSERT_TRUE(WIFEXITED(status)) << "status=" << status;
  EXPECT_EQ(WEXITSTATUS(status), kOk);
}

TEST(SeccompTest,
     TraceRereadsTracerModifiedSyscallAfterNonzeroResumeSignal) {
#if !defined(__x86_64__)
  GTEST_SKIP() << "register rewrite is x86_64-specific";
#else
  int ready[2] = {};
  int release[2] = {};
  int payload[2] = {};
  ASSERT_EQ(0, pipe(ready));
  ASSERT_EQ(0, pipe(release));
  ASSERT_EQ(0, pipe(payload));

  const pid_t child = fork();
  ASSERT_GE(child, 0);
  if (child == 0) {
    close(ready[0]);
    close(release[1]);
    close(payload[0]);
    if (signal(SIGUSR1, SIG_IGN) == SIG_ERR) _exit(69);
    if (PtraceCall(PTRACE_TRACEME, 0, 0, 0) != 0) _exit(70);
    raise(SIGSTOP);

    RequireNoNewPrivs();
    struct sock_filter filter[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_getpid, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_TRACE | 0x1234),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    if (InstallFilter(filter, sizeof(filter) / sizeof(filter[0])) != 0) {
      _exit(71);
    }
    const char marker = 'R';
    if (write(ready[1], &marker, 1) != 1) _exit(72);
    char token = 0;
    if (read(release[0], &token, 1) != 1) _exit(73);

    errno = 0;
    const long result = syscall(__NR_getpid);
    _exit(result == 1 && errno == 0 ? 0 : 74);
  }
  ScopedChild child_guard(child);
  close(ready[1]);
  close(release[0]);
  close(payload[1]);

  int status = 0;
  ASSERT_EQ(child, WaitPidDeadline(child, &status));
  ASSERT_TRUE(WIFSTOPPED(status));
  ASSERT_EQ(SIGSTOP, WSTOPSIG(status));
  ASSERT_EQ(0, PtraceCall(PTRACE_SETOPTIONS, child, 0,
                          PTRACE_O_TRACESECCOMP));
  ASSERT_EQ(0, PtraceCall(PTRACE_CONT, child, 0, 0));

  char marker = 0;
  ASSERT_TRUE(ReadByteDeadline(ready[0], &marker));
  ASSERT_EQ('R', marker);
  const char go = 'G';
  ASSERT_EQ(1, write(release[1], &go, 1));

  ASSERT_EQ(child, WaitPidDeadline(child, &status));
  ASSERT_TRUE(WIFSTOPPED(status));
  ASSERT_EQ(SIGTRAP | (PTRACE_EVENT_SECCOMP << 8), status >> 8);

  user_regs_struct regs = {};
  ASSERT_EQ(0, PtraceCall(PTRACE_GETREGS, child, 0,
                          reinterpret_cast<unsigned long>(&regs)));
  ASSERT_EQ(static_cast<unsigned long>(__NR_getpid), regs.orig_rax);
  regs.orig_rax = __NR_write;
  regs.rdi = payload[1];
  regs.rsi = reinterpret_cast<unsigned long>(&g_trace_payload);
  regs.rdx = 1;
  ASSERT_EQ(0, PtraceCall(PTRACE_SETREGS, child, 0,
                          reinterpret_cast<unsigned long>(&regs)));
  // A nonzero resume signal is independent of whether this already-published
  // SECCOMP event belonged to the live tracing session. SIGUSR1 is ignored so
  // Linux and DragonOS can differ in whether the signal is actually delivered
  // without changing the oracle for the rewritten syscall.
  ASSERT_EQ(0, PtraceCall(PTRACE_CONT, child, 0, SIGUSR1));

  char observed = 0;
  ASSERT_TRUE(ReadByteDeadline(payload[0], &observed));
  EXPECT_EQ(g_trace_payload, observed);
  ASSERT_EQ(child, WaitPidDeadline(child, &status));
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
  child_guard.Release();
  close(ready[0]);
  close(release[1]);
  close(payload[0]);
#endif
}

TEST(SeccompTest, TsyncAppliesFilterToSiblingThread) {
  int status = ChildStatus([] {
    RequireNoNewPrivs();

    std::atomic<int> ready{0};
    pthread_t thread;
    if (pthread_create(&thread, nullptr, SeccompTsyncThread, &ready) != 0) {
      _exit(2);
    }

    struct sock_filter filter[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_getpid, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | ENOTNAM),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    if (InstallFilterWithFlags(filter, sizeof(filter) / sizeof(filter[0]),
                               SECCOMP_FILTER_FLAG_TSYNC) != 0) {
      _exit(3);
    }

    ready.store(1, std::memory_order_release);
    void* result = nullptr;
    if (pthread_join(thread, &result) != 0) {
      _exit(4);
    }
    _exit(reinterpret_cast<intptr_t>(result) == kOk ? kOk : 5);
  });

  ASSERT_TRUE(WIFEXITED(status)) << "status=" << status;
  EXPECT_EQ(WEXITSTATUS(status), kOk);
}

TEST(SeccompTest, TrapIsForcedWhenSigsysIgnored) {
  int status = ChildStatus([] {
    signal(SIGSYS, SIG_IGN);
    RequireNoNewPrivs();
    InstallGetpidTrapFilter();
    syscall(__NR_getpid);
    _exit(3);
  });

  ASSERT_TRUE(WIFSIGNALED(status)) << "status=" << status;
  EXPECT_EQ(WTERMSIG(status), SIGSYS);
}

TEST(SeccompTest, TrapIsForcedWhenSigsysBlocked) {
  int status = ChildStatus([] {
    struct sigaction sa = {};
    sa.sa_sigaction = SeccompTrapHandler;
    sa.sa_flags = SA_SIGINFO;
    if (sigaction(SIGSYS, &sa, nullptr) != 0) {
      _exit(2);
    }

    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGSYS);
    if (sigprocmask(SIG_BLOCK, &set, nullptr) != 0) {
      _exit(2);
    }

    RequireNoNewPrivs();
    InstallGetpidTrapFilter();
    syscall(__NR_getpid);
    _exit(3);
  });

  ASSERT_TRUE(WIFSIGNALED(status)) << "status=" << status;
  EXPECT_EQ(WTERMSIG(status), SIGSYS);
}

TEST(SeccompTest, UnalignedUserFilterPointerIsParsedSafely) {
  int status = ChildStatus([] {
    RequireNoNewPrivs();
    struct sock_filter allow[] = {
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    alignas(1) unsigned char raw[sizeof(allow) + 1];
    memset(raw, 0, sizeof(raw));
    memcpy(raw + 1, allow, sizeof(allow));

    struct sock_fprog prog = {
        .len = 1,
        .filter = reinterpret_cast<struct sock_filter*>(raw + 1),
    };
    if (syscall(__NR_seccomp, SECCOMP_SET_MODE_FILTER, 0, &prog) != 0) {
      _exit(2);
    }
    syscall(__NR_getpid);
    _exit(kOk);
  });

  ASSERT_TRUE(WIFEXITED(status)) << "status=" << status;
  EXPECT_EQ(WEXITSTATUS(status), kOk);
}

TEST(SeccompTest, RejectsSocketOnlyAndModuloOpcodes) {
  int status = ChildStatus([] {
    RequireNoNewPrivs();

    struct sock_filter mod_k[] = {
        BPF_STMT(BPF_LD | BPF_IMM, 7),
        BPF_STMT(BPF_ALU | BPF_MOD | BPF_K, 3),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    errno = 0;
    if (InstallFilter(mod_k, sizeof(mod_k) / sizeof(mod_k[0])) != -1 ||
        errno != EINVAL) {
      _exit(3);
    }

    struct sock_filter mod_x[] = {
        BPF_STMT(BPF_LDX | BPF_IMM, 3),
        BPF_STMT(BPF_LD | BPF_IMM, 7),
        BPF_STMT(BPF_ALU | BPF_MOD | BPF_X, 0),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    errno = 0;
    if (InstallFilter(mod_x, sizeof(mod_x) / sizeof(mod_x[0])) != -1 ||
        errno != EINVAL) {
      _exit(4);
    }

    struct sock_filter packet_indirect[] = {
        BPF_STMT(BPF_LDX | BPF_IMM, 0),
        BPF_STMT(BPF_LD | BPF_W | BPF_IND, 0),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    errno = 0;
    if (InstallFilter(packet_indirect,
                      sizeof(packet_indirect) / sizeof(packet_indirect[0])) != -1 ||
        errno != EINVAL) {
      _exit(5);
    }
    _exit(kOk);
  });

  ASSERT_TRUE(WIFEXITED(status)) << "status=" << status;
  EXPECT_EQ(WEXITSTATUS(status), kOk);
}

int main(int argc, char** argv) {
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
