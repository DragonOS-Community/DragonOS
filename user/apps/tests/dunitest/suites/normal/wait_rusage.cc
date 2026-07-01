#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include "gtest/gtest.h"

#ifndef PTRACE_TRACEME
#define PTRACE_TRACEME 0
#endif

#ifndef __WCLONE
#define __WCLONE 0x80000000
#endif

#ifndef __WALL
#define __WALL 0x40000000
#endif

#ifndef __WNOTHREAD
#define __WNOTHREAD 0x20000000
#endif

#ifndef P_PIDFD
#define P_PIDFD 3
#endif

#ifndef CLONE_PARENT
#define CLONE_PARENT 0x00008000
#endif

namespace {

constexpr size_t kCloneStackSize = 1024 * 1024;
constexpr uid_t kWaitidChildUid = 1234;

bool ReadExact(int fd, void* buf, size_t len) {
  char* cursor = static_cast<char*>(buf);
  while (len > 0) {
    ssize_t n = read(fd, cursor, len);
    if (n <= 0) {
      return false;
    }
    cursor += n;
    len -= static_cast<size_t>(n);
  }
  return true;
}

bool WriteExact(int fd, const void* buf, size_t len) {
  const char* cursor = static_cast<const char*>(buf);
  while (len > 0) {
    ssize_t n = write(fd, cursor, len);
    if (n <= 0) {
      return false;
    }
    cursor += n;
    len -= static_cast<size_t>(n);
  }
  return true;
}

uint64_t RusageCpuUsec(const struct rusage& ru) {
  return static_cast<uint64_t>(ru.ru_utime.tv_sec) * 1000000ULL +
         static_cast<uint64_t>(ru.ru_utime.tv_usec) +
         static_cast<uint64_t>(ru.ru_stime.tv_sec) * 1000000ULL +
         static_cast<uint64_t>(ru.ru_stime.tv_usec);
}

uint64_t MonotonicUsec() {
  struct timespec ts;
  if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) {
    return 0;
  }
  return static_cast<uint64_t>(ts.tv_sec) * 1000000ULL +
         static_cast<uint64_t>(ts.tv_nsec) / 1000ULL;
}

uint64_t BurnCpuForUsec(uint64_t usec) {
  const uint64_t start = MonotonicUsec();
  volatile uint64_t sink = 0;
  while (MonotonicUsec() - start < usec) {
    for (int i = 0; i < 10000; ++i) {
      sink += static_cast<uint64_t>(i);
    }
  }
  return sink;
}

void BusyForUsec(uint64_t usec) {
  uint64_t sink = BurnCpuForUsec(usec);
  _exit(static_cast<int>(sink & 0));
}

void* ThreadBurn(void* arg) {
  BurnCpuForUsec(reinterpret_cast<uintptr_t>(arg));
  return nullptr;
}

void ExpectEncodedExitStatus(int status, int code) {
  ASSERT_TRUE(WIFEXITED(status)) << status;
  EXPECT_EQ(code, WEXITSTATUS(status));
  EXPECT_NE(code, status);
}

struct ThreadForkArgs {
  int ready_fd = -1;
  int release_fd = -1;
  pid_t child = -1;
  int fork_errno = 0;
  pid_t wait_result = -1;
  int wait_errno = 0;
  int wait_status = 0;
};

void* ForkChildFromThread(void* arg) {
  auto* args = reinterpret_cast<ThreadForkArgs*>(arg);
  pid_t child = fork();
  if (child == 0) {
    _exit(17);
  }
  if (child < 0) {
    args->fork_errno = errno;
  } else {
    args->child = child;
  }

  char byte = child < 0 ? 'e' : 'x';
  if (write(args->ready_fd, &byte, 1) != 1) {
    args->fork_errno = errno;
  }

  if (child >= 0) {
    char release = 0;
    if (read(args->release_fd, &release, 1) != 1) {
      args->fork_errno = errno;
      return nullptr;
    }
    args->wait_result = wait4(child, &args->wait_status, __WNOTHREAD, nullptr);
    if (args->wait_result < 0) {
      args->wait_errno = errno;
    }
  }
  return nullptr;
}

struct ThreadForkExitArgs {
  int ready_fd = -1;
  int release_fd = -1;
  pid_t child = -1;
  int fork_errno = 0;
};

void* ForkChildAndExitThread(void* arg) {
  auto* args = reinterpret_cast<ThreadForkExitArgs*>(arg);
  pid_t child = fork();
  if (child == 0) {
    char release = 0;
    if (read(args->release_fd, &release, 1) != 1) {
      _exit(22);
    }
    _exit(23);
  }
  if (child < 0) {
    args->fork_errno = errno;
  } else {
    args->child = child;
  }

  char byte = child < 0 ? 'e' : 'x';
  if (write(args->ready_fd, &byte, 1) != 1) {
    args->fork_errno = errno;
  }
  return nullptr;
}

struct ThreadTidArgs {
  int ready_fd = -1;
  int release_fd = -1;
  pid_t tid = -1;
};

void* ReportTidAndWait(void* arg) {
  auto* args = reinterpret_cast<ThreadTidArgs*>(arg);
  args->tid = static_cast<pid_t>(syscall(SYS_gettid));
  char byte = 't';
  if (write(args->ready_fd, &byte, 1) != 1) {
    return nullptr;
  }
  char release = 0;
  if (read(args->release_fd, &release, 1) != 1) {
    return nullptr;
  }
  return nullptr;
}

struct BlockingThreadExitArgs {
  int ready_fd = -1;
  int release_fd = -1;
};

void* BlockThenExitThread(void* arg) {
  auto* args = reinterpret_cast<BlockingThreadExitArgs*>(arg);
  char byte = 'r';
  if (write(args->ready_fd, &byte, 1) != 1) {
    syscall(SYS_exit, 4);
  }
  char release = 0;
  if (read(args->release_fd, &release, 1) != 1) {
    syscall(SYS_exit, 5);
  }
  syscall(SYS_exit, 0);
  return nullptr;
}

struct ThreadPtraceForkArgs {
  int result = -1;
  int err = 0;
  int status = 0;
};

void* ForkTracemeAndWaitFromThread(void* arg) {
  auto* args = reinterpret_cast<ThreadPtraceForkArgs*>(arg);
  int fds[2] = {};
  if (pipe(fds) != 0) {
    args->err = errno;
    return nullptr;
  }

  pid_t child = fork();
  if (child == 0) {
    close(fds[0]);
    if (syscall(SYS_ptrace, PTRACE_TRACEME, 0, 0, 0) != 0) {
      _exit(2);
    }
    if (write(fds[1], "x", 1) != 1) {
      _exit(3);
    }
    close(fds[1]);
    _exit(0);
  }
  if (child < 0) {
    args->err = errno;
    close(fds[0]);
    close(fds[1]);
    return nullptr;
  }

  close(fds[1]);
  char byte = 0;
  if (read(fds[0], &byte, 1) != 1) {
    args->err = errno;
    close(fds[0]);
    return nullptr;
  }
  close(fds[0]);

  args->result = wait4(child, &args->status, __WNOTHREAD | __WCLONE, nullptr);
  if (args->result < 0) {
    args->err = errno;
  }
  return nullptr;
}

struct CloneParentSpawnResult {
  pid_t pid = -1;
  int err = 0;
};

struct CloneParentWaitResult {
  pid_t result = -1;
  int err = 0;
  int status = 0;
};

int CloneParentChild(void* arg) {
  int release_fd = *reinterpret_cast<int*>(arg);
  char byte = 0;
  if (read(release_fd, &byte, 1) != 1) {
    _exit(18);
  }
  _exit(19);
}

bool WaitidPeekExited(pid_t child, siginfo_t* si) {
  for (int i = 0; i < 1000; ++i) {
    memset(si, 0, sizeof(*si));
    if (syscall(SYS_waitid, P_PID, child, si, WEXITED | WNOWAIT | WNOHANG,
                nullptr) != 0) {
      return false;
    }
    if (si->si_pid == child) {
      return true;
    }
    usleep(1000);
  }
  return false;
}

}  // namespace

TEST(WaitRusage, WaitidReportsChildRealUid) {
  pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    if (setuid(kWaitidChildUid) != 0) {
      _exit(77);
    }
    _exit(0);
  }

  siginfo_t si {};
  ASSERT_EQ(0, syscall(SYS_waitid, P_PID, child, &si, WEXITED, nullptr))
      << strerror(errno);
  EXPECT_EQ(SIGCHLD, si.si_signo);
  EXPECT_EQ(CLD_EXITED, si.si_code);
  EXPECT_EQ(child, si.si_pid);
  EXPECT_EQ(kWaitidChildUid, si.si_uid);
  EXPECT_EQ(0, si.si_status);
}

TEST(WaitRusage, WaitPidSelfReturnsEchild) {
  int status = 0;
  errno = 0;
  EXPECT_EQ(-1, wait4(getpid(), &status, WNOHANG, nullptr));
  EXPECT_EQ(ECHILD, errno);

  siginfo_t si {};
  errno = 0;
  EXPECT_EQ(-1, syscall(SYS_waitid, P_PID, getpid(), &si,
                        WEXITED | WNOHANG, nullptr));
  EXPECT_EQ(ECHILD, errno);
}

TEST(WaitRusage, CloneParentChildNotWaitableByCreatorWithWnothread) {
  int child_pid_pipe[2] = {};
  int child_release_pipe[2] = {};
  int creator_attempt_pipe[2] = {};
  int creator_result_pipe[2] = {};
  ASSERT_EQ(0, pipe(child_pid_pipe)) << strerror(errno);
  ASSERT_EQ(0, pipe(child_release_pipe)) << strerror(errno);
  ASSERT_EQ(0, pipe(creator_attempt_pipe)) << strerror(errno);
  ASSERT_EQ(0, pipe(creator_result_pipe)) << strerror(errno);

  pid_t creator = fork();
  ASSERT_GE(creator, 0) << strerror(errno);
  if (creator == 0) {
    close(child_pid_pipe[0]);
    close(creator_attempt_pipe[1]);
    close(creator_result_pipe[0]);
    close(child_release_pipe[1]);

    void* stack = malloc(kCloneStackSize);
    CloneParentSpawnResult spawn {};
    if (stack == nullptr) {
      spawn.err = ENOMEM;
      WriteExact(child_pid_pipe[1], &spawn, sizeof(spawn));
      _exit(10);
    }

    void* stack_top = static_cast<char*>(stack) + kCloneStackSize;
    pid_t child = clone(CloneParentChild, stack_top, CLONE_PARENT | SIGCHLD,
                        &child_release_pipe[0]);
    spawn.pid = child;
    if (child < 0) {
      spawn.err = errno;
      WriteExact(child_pid_pipe[1], &spawn, sizeof(spawn));
      _exit(11);
    }
    if (!WriteExact(child_pid_pipe[1], &spawn, sizeof(spawn))) {
      _exit(12);
    }

    char attempt = 0;
    if (read(creator_attempt_pipe[0], &attempt, 1) != 1) {
      _exit(13);
    }

    CloneParentWaitResult result {};
    result.result = wait4(child, &result.status, __WNOTHREAD | WNOHANG,
                          nullptr);
    if (result.result < 0) {
      result.err = errno;
    }
    if (!WriteExact(creator_result_pipe[1], &result, sizeof(result))) {
      _exit(14);
    }
    _exit(0);
  }

  close(child_pid_pipe[1]);
  close(creator_attempt_pipe[0]);
  close(creator_result_pipe[1]);
  close(child_release_pipe[0]);

  CloneParentSpawnResult spawn {};
  ASSERT_TRUE(ReadExact(child_pid_pipe[0], &spawn, sizeof(spawn)))
      << strerror(errno);
  ASSERT_GT(spawn.pid, 0) << strerror(spawn.err);

  ASSERT_EQ(1, write(child_release_pipe[1], "x", 1)) << strerror(errno);
  siginfo_t si {};
  ASSERT_TRUE(WaitidPeekExited(spawn.pid, &si)) << strerror(errno);
  EXPECT_EQ(CLD_EXITED, si.si_code);
  EXPECT_EQ(19, si.si_status);

  ASSERT_EQ(1, write(creator_attempt_pipe[1], "x", 1)) << strerror(errno);
  CloneParentWaitResult result {};
  ASSERT_TRUE(ReadExact(creator_result_pipe[0], &result, sizeof(result)))
      << strerror(errno);
  EXPECT_EQ(-1, result.result);
  EXPECT_EQ(ECHILD, result.err);

  int status = 0;
  ASSERT_EQ(spawn.pid, wait4(spawn.pid, &status, 0, nullptr)) << strerror(errno);
  ExpectEncodedExitStatus(status, 19);

  ASSERT_EQ(creator, wait4(creator, &status, 0, nullptr)) << strerror(errno);
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(WaitRusage, PtraceTracemeChildIsWaitableWithWclone) {
  int fds[2] = {};
  ASSERT_EQ(0, pipe(fds)) << strerror(errno);

  pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    close(fds[0]);
    if (syscall(SYS_ptrace, PTRACE_TRACEME, 0, 0, 0) != 0) {
      _exit(2);
    }
    if (write(fds[1], "x", 1) != 1) {
      _exit(3);
    }
    close(fds[1]);
    _exit(0);
  }

  close(fds[1]);
  char byte = 0;
  ASSERT_EQ(1, read(fds[0], &byte, 1)) << strerror(errno);
  close(fds[0]);

  int status = 0;
  ASSERT_EQ(child, wait4(child, &status, __WCLONE, nullptr)) << strerror(errno);
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));

  errno = 0;
  EXPECT_EQ(-1, wait4(child, nullptr, WNOHANG, nullptr));
  EXPECT_EQ(ECHILD, errno);
}

TEST(WaitRusage, RepeatedPtraceTracemeFailsWithEperm) {
  pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    if (syscall(SYS_ptrace, PTRACE_TRACEME, 0, 0, 0) != 0) {
      _exit(2);
    }
    errno = 0;
    if (syscall(SYS_ptrace, PTRACE_TRACEME, 0, 0, 0) != -1 ||
        errno != EPERM) {
      _exit(3);
    }
    _exit(0);
  }

  int status = 0;
  ASSERT_EQ(child, wait4(child, &status, __WALL, nullptr)) << strerror(errno);
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(WaitRusage, WcloneDoesNotReapOrdinaryForkChild) {
  pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    _exit(0);
  }

  int status = 0;
  errno = 0;
  EXPECT_EQ(-1, wait4(child, &status, WNOHANG | __WCLONE, nullptr));
  EXPECT_EQ(ECHILD, errno);

  ASSERT_EQ(child, wait4(child, &status, 0, nullptr)) << strerror(errno);
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(WaitRusage, Wait4AndWaitpidReturnEncodedExitStatus) {
  pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    _exit(42);
  }

  int status = 0;
  ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
  ExpectEncodedExitStatus(status, 42);

  child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    _exit(21);
  }

  status = 0;
  ASSERT_EQ(child, wait4(child, &status, 0, nullptr)) << strerror(errno);
  ExpectEncodedExitStatus(status, 21);

  child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    _exit(33);
  }

  status = 0;
  ASSERT_EQ(child, wait4(-1, &status, 0, nullptr)) << strerror(errno);
  ExpectEncodedExitStatus(status, 33);

  child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    _exit(34);
  }

  status = 0;
  ASSERT_EQ(child, wait4(0, &status, 0, nullptr)) << strerror(errno);
  ExpectEncodedExitStatus(status, 34);

  int fds[2] = {};
  ASSERT_EQ(0, pipe(fds)) << strerror(errno);
  child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    close(fds[1]);
    char byte = 0;
    if (read(fds[0], &byte, 1) < 0) {
      _exit(2);
    }
    close(fds[0]);
    _exit(35);
  }

  close(fds[0]);
  ASSERT_EQ(0, setpgid(child, child)) << strerror(errno);
  ASSERT_EQ(1, write(fds[1], "x", 1)) << strerror(errno);
  close(fds[1]);

  status = 0;
  ASSERT_EQ(child, wait4(-child, &status, 0, nullptr)) << strerror(errno);
  ExpectEncodedExitStatus(status, 35);
}

TEST(WaitRusage, WaitidPidExitedChildWithoutWexitedReturnsEchild) {
  pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    _exit(7);
  }

  siginfo_t si {};
  bool observed_exit = false;
  for (int i = 0; i < 1000; ++i) {
    memset(&si, 0, sizeof(si));
    ASSERT_EQ(0, syscall(SYS_waitid, P_PID, child, &si,
                         WEXITED | WNOWAIT | WNOHANG, nullptr))
        << strerror(errno);
    if (si.si_pid == child) {
      observed_exit = true;
      break;
    }
    usleep(1000);
  }
  ASSERT_TRUE(observed_exit);
  EXPECT_EQ(CLD_EXITED, si.si_code);
  EXPECT_EQ(7, si.si_status);

  memset(&si, 0x5a, sizeof(si));
  errno = 0;
  EXPECT_EQ(-1,
            syscall(SYS_waitid, P_PID, child, &si, WSTOPPED | WNOHANG, nullptr));
  EXPECT_EQ(ECHILD, errno);

  int status = 0;
  ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
  ExpectEncodedExitStatus(status, 7);
}

TEST(WaitRusage, WnothreadWaitsForChildForkedByCurrentThread) {
  int ready_pipe[2] = {};
  int release_pipe[2] = {};
  ASSERT_EQ(0, pipe(ready_pipe)) << strerror(errno);
  ASSERT_EQ(0, pipe(release_pipe)) << strerror(errno);

  ThreadForkArgs args;
  args.ready_fd = ready_pipe[1];
  args.release_fd = release_pipe[0];
  pthread_t thread {};
  ASSERT_EQ(0, pthread_create(&thread, nullptr, ForkChildFromThread, &args))
      << strerror(errno);

  char byte = 0;
  ASSERT_EQ(1, read(ready_pipe[0], &byte, 1)) << strerror(errno);
  close(ready_pipe[0]);
  ASSERT_EQ(0, args.fork_errno) << strerror(args.fork_errno);
  ASSERT_GT(args.child, 0);

  int status = 0;
  errno = 0;
  EXPECT_EQ(-1, wait4(args.child, &status, WNOHANG | __WNOTHREAD, nullptr));
  EXPECT_EQ(ECHILD, errno);

  ASSERT_EQ(1, write(release_pipe[1], "x", 1)) << strerror(errno);
  close(release_pipe[1]);
  ASSERT_EQ(0, pthread_join(thread, nullptr)) << strerror(errno);
  close(ready_pipe[1]);
  close(release_pipe[0]);
  EXPECT_EQ(args.child, args.wait_result) << strerror(args.wait_errno);
  ExpectEncodedExitStatus(args.wait_status, 17);
}

TEST(WaitRusage, WnothreadChildReparentedWhenForkingThreadExits) {
  int ready_pipe[2] = {};
  int release_pipe[2] = {};
  ASSERT_EQ(0, pipe(ready_pipe)) << strerror(errno);
  ASSERT_EQ(0, pipe(release_pipe)) << strerror(errno);

  ThreadForkExitArgs args;
  args.ready_fd = ready_pipe[1];
  args.release_fd = release_pipe[0];
  pthread_t thread {};
  ASSERT_EQ(0, pthread_create(&thread, nullptr, ForkChildAndExitThread, &args))
      << strerror(errno);

  char byte = 0;
  ASSERT_EQ(1, read(ready_pipe[0], &byte, 1)) << strerror(errno);
  close(ready_pipe[0]);
  ASSERT_EQ(0, pthread_join(thread, nullptr)) << strerror(errno);
  close(ready_pipe[1]);
  close(release_pipe[0]);

  ASSERT_EQ(0, args.fork_errno) << strerror(args.fork_errno);
  ASSERT_GT(args.child, 0);

  ASSERT_EQ(1, write(release_pipe[1], "x", 1)) << strerror(errno);
  close(release_pipe[1]);

  int status = 0;
  ASSERT_EQ(args.child, wait4(args.child, &status, __WNOTHREAD, nullptr))
      << strerror(errno);
  ExpectEncodedExitStatus(status, 23);
}

TEST(WaitRusage, ThreadGroupLeaderWaitDelayedUntilSubthreadsExit) {
  int ready_pipe[2] = {};
  int release_pipe[2] = {};
  ASSERT_EQ(0, pipe(ready_pipe)) << strerror(errno);
  ASSERT_EQ(0, pipe(release_pipe)) << strerror(errno);

  pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    close(ready_pipe[0]);
    close(release_pipe[1]);
    BlockingThreadExitArgs args;
    args.ready_fd = ready_pipe[1];
    args.release_fd = release_pipe[0];
    pthread_t thread {};
    if (pthread_create(&thread, nullptr, BlockThenExitThread, &args) != 0) {
      syscall(SYS_exit, 2);
    }
    syscall(SYS_exit, 0);
  }

  close(ready_pipe[1]);
  close(release_pipe[0]);
  char byte = 0;
  ASSERT_EQ(1, read(ready_pipe[0], &byte, 1)) << strerror(errno);
  close(ready_pipe[0]);

  usleep(50000);
  siginfo_t si {};
  ASSERT_EQ(0, syscall(SYS_waitid, P_PID, child, &si, WSTOPPED | WNOHANG,
                       nullptr))
      << strerror(errno);
  EXPECT_EQ(0, si.si_pid);

  int status = 0;
  ASSERT_EQ(0, wait4(child, &status, WNOHANG, nullptr)) << strerror(errno);

  ASSERT_EQ(1, write(release_pipe[1], "x", 1)) << strerror(errno);
  close(release_pipe[1]);

  ASSERT_EQ(child, wait4(child, &status, 0, nullptr)) << strerror(errno);
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(WaitRusage, ThreadForkedTracemeIsWaitableByForkingThreadWithWclone) {
  ThreadPtraceForkArgs args;
  pthread_t thread {};
  ASSERT_EQ(0, pthread_create(&thread, nullptr, ForkTracemeAndWaitFromThread, &args))
      << strerror(errno);
  ASSERT_EQ(0, pthread_join(thread, nullptr)) << strerror(errno);

  EXPECT_GT(args.result, 0) << strerror(args.err);
  ASSERT_TRUE(WIFEXITED(args.status));
  EXPECT_EQ(0, WEXITSTATUS(args.status));
}

TEST(WaitRusage, NaturalWaitCannotReapThreadTid) {
  int ready_pipe[2] = {};
  int release_pipe[2] = {};
  ASSERT_EQ(0, pipe(ready_pipe)) << strerror(errno);
  ASSERT_EQ(0, pipe(release_pipe)) << strerror(errno);

  ThreadTidArgs args;
  args.ready_fd = ready_pipe[1];
  args.release_fd = release_pipe[0];
  pthread_t thread {};
  ASSERT_EQ(0, pthread_create(&thread, nullptr, ReportTidAndWait, &args))
      << strerror(errno);

  char byte = 0;
  ASSERT_EQ(1, read(ready_pipe[0], &byte, 1)) << strerror(errno);
  close(ready_pipe[0]);
  ASSERT_GT(args.tid, 0);

  int status = 0;
  errno = 0;
  EXPECT_EQ(-1, wait4(args.tid, &status, WNOHANG | __WCLONE, nullptr));
  EXPECT_EQ(ECHILD, errno);

  errno = 0;
  EXPECT_EQ(-1, wait4(args.tid, &status, WNOHANG | __WALL, nullptr));
  EXPECT_EQ(ECHILD, errno);

  ASSERT_EQ(1, write(release_pipe[1], "x", 1)) << strerror(errno);
  close(release_pipe[1]);
  ASSERT_EQ(0, pthread_join(thread, nullptr)) << strerror(errno);
  close(ready_pipe[1]);
  close(release_pipe[0]);
}

TEST(WaitRusage, WaitidPidfdNegativeFdIsEinval) {
  siginfo_t si {};
  errno = 0;
  EXPECT_EQ(-1,
            syscall(SYS_waitid, P_PIDFD, -1, &si, WEXITED | WNOHANG, nullptr));
  EXPECT_EQ(EINVAL, errno);
}

TEST(WaitRusage, Wait4RejectsWnowaitWithoutReapingChild) {
  pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    _exit(0);
  }

  int status = 0;
  errno = 0;
  EXPECT_EQ(-1, wait4(child, &status, WNOWAIT, nullptr));
  EXPECT_EQ(EINVAL, errno);

  ASSERT_EQ(child, wait4(child, &status, 0, nullptr)) << strerror(errno);
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(WaitRusage, WnohangNoEventDoesNotTouchUserPointers) {
  int fds[2] = {};
  ASSERT_EQ(0, pipe(fds)) << strerror(errno);

  pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    close(fds[1]);
    char byte = 0;
    if (read(fds[0], &byte, 1) < 0) {
      _exit(2);
    }
    close(fds[0]);
    _exit(0);
  }
  close(fds[0]);

  errno = 0;
  EXPECT_EQ(0, wait4(child, reinterpret_cast<int*>(1), WNOHANG,
                     reinterpret_cast<struct rusage*>(1)))
      << strerror(errno);

  ASSERT_EQ(1, write(fds[1], "x", 1)) << strerror(errno);
  close(fds[1]);

  int status = 0;
  ASSERT_EQ(child, wait4(child, &status, 0, nullptr)) << strerror(errno);
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(WaitRusage, ExplicitSigignSigchldAutoreapsWithoutChildRusage) {
  struct sigaction old_action {};
  struct sigaction ignore_action {};
  ignore_action.sa_handler = SIG_IGN;
  sigemptyset(&ignore_action.sa_mask);
  ASSERT_EQ(0, sigaction(SIGCHLD, &ignore_action, &old_action)) << strerror(errno);

  struct rusage before {};
  ASSERT_EQ(0, getrusage(RUSAGE_CHILDREN, &before)) << strerror(errno);

  pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    BusyForUsec(300000);
  }

  errno = 0;
  EXPECT_EQ(-1, wait4(child, nullptr, 0, nullptr));
  EXPECT_EQ(ECHILD, errno);

  struct rusage after {};
  ASSERT_EQ(0, getrusage(RUSAGE_CHILDREN, &after)) << strerror(errno);
  EXPECT_EQ(RusageCpuUsec(before), RusageCpuUsec(after));

  ASSERT_EQ(0, sigaction(SIGCHLD, &old_action, nullptr)) << strerror(errno);
}

TEST(WaitRusage, WNowaitDoesNotReapAndWait4AccountsChildUsage) {
  struct rusage before {};
  ASSERT_EQ(0, getrusage(RUSAGE_CHILDREN, &before)) << strerror(errno);

  pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    BusyForUsec(500000);
  }

  siginfo_t si {};
  struct rusage nowait_usage {};
  ASSERT_EQ(0, syscall(SYS_waitid, P_PID, child, &si, WEXITED | WNOWAIT,
                       &nowait_usage))
      << strerror(errno);
  EXPECT_EQ(SIGCHLD, si.si_signo);
  EXPECT_EQ(CLD_EXITED, si.si_code);
  EXPECT_EQ(0, si.si_status);
  EXPECT_EQ(child, si.si_pid);
  EXPECT_GT(RusageCpuUsec(nowait_usage), 0u);

  struct rusage after_nowait {};
  ASSERT_EQ(0, getrusage(RUSAGE_CHILDREN, &after_nowait)) << strerror(errno);
  EXPECT_EQ(RusageCpuUsec(before), RusageCpuUsec(after_nowait));

  int status = 0;
  struct rusage waited_usage {};
  ASSERT_EQ(child, wait4(child, &status, 0, &waited_usage)) << strerror(errno);
  EXPECT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
  EXPECT_GT(RusageCpuUsec(waited_usage), 0u);

  struct rusage after_wait {};
  ASSERT_EQ(0, getrusage(RUSAGE_CHILDREN, &after_wait)) << strerror(errno);
  EXPECT_GE(RusageCpuUsec(after_wait) - RusageCpuUsec(before),
            RusageCpuUsec(waited_usage));

  errno = 0;
  EXPECT_EQ(-1, wait4(child, nullptr, WNOHANG, nullptr));
  EXPECT_EQ(ECHILD, errno);
}

TEST(WaitRusage, Wait4IncludesExitedThreadCpuTime) {
  pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    pthread_t worker {};
    if (pthread_create(&worker, nullptr, ThreadBurn,
                       reinterpret_cast<void*>(static_cast<uintptr_t>(600000))) != 0) {
      _exit(2);
    }
    if (pthread_join(worker, nullptr) != 0) {
      _exit(3);
    }
    _exit(0);
  }

  int status = 0;
  struct rusage usage {};
  ASSERT_EQ(child, wait4(child, &status, 0, &usage)) << strerror(errno);
  ASSERT_TRUE(WIFEXITED(status));
  ASSERT_EQ(0, WEXITSTATUS(status));
  EXPECT_GE(RusageCpuUsec(usage), 100000u);
}

int main(int argc, char** argv) {
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
