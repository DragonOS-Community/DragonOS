#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <errno.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/ucontext.h>
#include <sys/wait.h>
#include <ucontext.h>
#include <unistd.h>

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

namespace {

constexpr int kOk = 42;
constexpr long kTrapReturn = 424242;

int InstallFilter(const struct sock_filter* filter, unsigned short len) {
  struct sock_fprog prog = {
      .len = len,
      .filter = const_cast<struct sock_filter*>(filter),
  };
  return syscall(__NR_seccomp, SECCOMP_SET_MODE_FILTER, 0, &prog);
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
      BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_TRAP | 123),
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
  if (info->si_signo != SIGSYS || info->si_code != SYS_SECCOMP || info->si_errno != 123) {
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

int main(int argc, char** argv) {
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
