#include <errno.h>
#include <fcntl.h>
#include <gtest/gtest.h>
#include <limits.h>
#include <sched.h>
#include <signal.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#include <array>
#include <cstdint>
#include <cstdlib>
#include <vector>

#ifndef SYS_close_range
#define SYS_close_range 436
#endif
#ifndef CLOSE_RANGE_UNSHARE
#define CLOSE_RANGE_UNSHARE (1U << 1)
#endif
#ifndef CLOSE_RANGE_CLOEXEC
#define CLOSE_RANGE_CLOEXEC (1U << 2)
#endif

namespace {

int CloseRange(unsigned int first, unsigned int last, unsigned int flags) {
  return static_cast<int>(syscall(SYS_close_range, first, last, flags));
}

long RawCloseRange(uint64_t first, uint64_t last, uint64_t flags) {
  return syscall(SYS_close_range, first, last, flags);
}

class FdGuard {
 public:
  explicit FdGuard(int fd = -1) : fd_(fd) {}
  ~FdGuard() {
    if (fd_ >= 0) close(fd_);
  }
  FdGuard(const FdGuard&) = delete;
  FdGuard& operator=(const FdGuard&) = delete;
  FdGuard(FdGuard&& other) noexcept : fd_(other.release()) {}
  FdGuard& operator=(FdGuard&& other) noexcept {
    if (this != &other) {
      if (fd_ >= 0) close(fd_);
      fd_ = other.release();
    }
    return *this;
  }
  int get() const { return fd_; }
  int release() {
    int fd = fd_;
    fd_ = -1;
    return fd;
  }

 private:
  int fd_;
};

FdGuard OpenNull() { return FdGuard(open("/dev/null", O_RDWR)); }

bool IsOpen(int fd) { return fcntl(fd, F_GETFD) >= 0; }

bool HasCloexec(int fd) {
  int flags = fcntl(fd, F_GETFD);
  return flags >= 0 && (flags & FD_CLOEXEC) != 0;
}

struct SharedChildArgs {
  unsigned int first;
  unsigned int last;
  unsigned int flags;
  int target;
  int preserved;
  bool expect_closed;
  bool expect_cloexec;
};

int SharedChild(void* opaque) {
  auto* args = static_cast<SharedChildArgs*>(opaque);
  if (CloseRange(args->first, args->last, args->flags) != 0) return 10;
  if (args->expect_closed == IsOpen(args->target)) return 11;
  if (!args->expect_closed && args->expect_cloexec != HasCloexec(args->target)) return 12;
  if (args->preserved >= 0 && !IsOpen(args->preserved)) return 13;
  return 0;
}

int RunCloneFilesChild(SharedChildArgs* args) {
  constexpr size_t kStackSize = 64 * 1024;
  std::vector<unsigned char> stack(kStackSize);
  pid_t pid = clone(SharedChild, stack.data() + stack.size(), CLONE_FILES | SIGCHLD, args);
  if (pid < 0) return -1;
  int status = 0;
  if (waitpid(pid, &status, 0) != pid) return -1;
  if (!WIFEXITED(status)) return -1;
  return WEXITSTATUS(status);
}

TEST(CloseRangeSemantics, ValidationBoundsHolesAndRawU32Abi) {
  std::array<FdGuard, 6> fds = {OpenNull(), OpenNull(), OpenNull(),
                                OpenNull(), OpenNull(), OpenNull()};
  for (const auto& fd : fds) ASSERT_GE(fd.get(), 0);

  errno = 0;
  EXPECT_EQ(-1, CloseRange(fds[4].get(), fds[1].get(), 0));
  EXPECT_EQ(EINVAL, errno);
  errno = 0;
  EXPECT_EQ(-1, CloseRange(fds[1].get(), fds[4].get(), 0x80));
  EXPECT_EQ(EINVAL, errno);
  EXPECT_EQ(0, CloseRange(UINT_MAX, UINT_MAX, 0));

  ASSERT_EQ(0, close(fds[2].release()));
  ASSERT_EQ(0, CloseRange(fds[1].get(), fds[4].get(), 0));
  EXPECT_TRUE(IsOpen(fds[0].get()));
  for (int i = 1; i <= 4; ++i) EXPECT_FALSE(IsOpen(fds[i].get()));
  EXPECT_TRUE(IsOpen(fds[5].get()));
  EXPECT_EQ(0, CloseRange(fds[1].get(), fds[4].get(), 0));

  FdGuard raw = OpenNull();
  FdGuard sentinel = OpenNull();
  ASSERT_GE(raw.get(), 0);
  ASSERT_GE(sentinel.get(), 0);
  const uint64_t high = uint64_t{1} << 32;
  ASSERT_EQ(0, RawCloseRange(high | static_cast<uint32_t>(raw.get()),
                             high | static_cast<uint32_t>(raw.get()), high));
  EXPECT_FALSE(IsOpen(raw.get()));
  EXPECT_TRUE(IsOpen(sentinel.get()));
}

TEST(CloseRangeSemantics, CloseAndCloexecRanges) {
  std::array<FdGuard, 5> fds = {OpenNull(), OpenNull(), OpenNull(), OpenNull(), OpenNull()};
  for (const auto& fd : fds) ASSERT_GE(fd.get(), 0);

  ASSERT_EQ(0, CloseRange(fds[1].get(), fds[3].get(), CLOSE_RANGE_CLOEXEC));
  EXPECT_FALSE(HasCloexec(fds[0].get()));
  for (int i = 1; i <= 3; ++i) {
    EXPECT_TRUE(IsOpen(fds[i].get()));
    EXPECT_TRUE(HasCloexec(fds[i].get()));
  }
  EXPECT_FALSE(HasCloexec(fds[4].get()));

  int hole = fds[2].release();
  ASSERT_EQ(0, close(hole));
  ASSERT_EQ(0, CloseRange(hole, hole, CLOSE_RANGE_CLOEXEC));
  FdGuard reused = OpenNull();
  ASSERT_EQ(hole, reused.get());
  EXPECT_FALSE(HasCloexec(reused.get()));

  ASSERT_EQ(0, CloseRange(fds[3].get(), UINT_MAX, 0));
  EXPECT_TRUE(IsOpen(fds[0].get()));
  EXPECT_TRUE(IsOpen(fds[1].get()));
  EXPECT_FALSE(IsOpen(fds[3].get()));
  EXPECT_FALSE(IsOpen(fds[4].get()));
}

TEST(CloseRangeSemantics, SharedAndUnsharedTableSemantics) {
  FdGuard target = OpenNull();
  FdGuard preserved = OpenNull();
  ASSERT_GE(target.get(), 0);
  ASSERT_GE(preserved.get(), 0);

  SharedChildArgs private_close = {
      static_cast<unsigned int>(target.get()), static_cast<unsigned int>(target.get()),
      CLOSE_RANGE_UNSHARE, target.get(), preserved.get(), true, false};
  ASSERT_EQ(0, RunCloneFilesChild(&private_close));
  EXPECT_TRUE(IsOpen(target.get()));
  EXPECT_TRUE(IsOpen(preserved.get()));

  SharedChildArgs private_cloexec = {
      static_cast<unsigned int>(target.get()), static_cast<unsigned int>(target.get()),
      CLOSE_RANGE_UNSHARE | CLOSE_RANGE_CLOEXEC, target.get(), preserved.get(), false, true};
  ASSERT_EQ(0, RunCloneFilesChild(&private_cloexec));
  EXPECT_FALSE(HasCloexec(target.get()));

  SharedChildArgs shared_cloexec = {
      static_cast<unsigned int>(target.get()), static_cast<unsigned int>(target.get()),
      CLOSE_RANGE_CLOEXEC, target.get(), preserved.get(), false, true};
  ASSERT_EQ(0, RunCloneFilesChild(&shared_cloexec));
  EXPECT_TRUE(HasCloexec(target.get()));

  ASSERT_EQ(0, fcntl(target.get(), F_SETFD, 0));
  SharedChildArgs shared_close = {
      static_cast<unsigned int>(target.get()), static_cast<unsigned int>(target.get()),
      0, target.get(), preserved.get(), true, false};
  ASSERT_EQ(0, RunCloneFilesChild(&shared_close));
  EXPECT_FALSE(IsOpen(target.get()));
  EXPECT_TRUE(IsOpen(preserved.get()));
}

int SparseNextFdProcess() {
  FdGuard base = OpenNull();
  if (base.get() < 0) return 20;
  for (int fd = 3; fd < 128; ++fd) {
    if (fd != base.get() && dup2(base.get(), fd) != fd) return 21;
  }

  SharedChildArgs args = {64, UINT_MAX, CLOSE_RANGE_UNSHARE, 64, base.get(), true, false};
  constexpr size_t kStackSize = 64 * 1024;
  std::vector<unsigned char> stack(kStackSize);
  auto child_fn = [](void* opaque) -> int {
    auto* child_args = static_cast<SharedChildArgs*>(opaque);
    if (CloseRange(child_args->first, child_args->last, child_args->flags) != 0) return 22;
    int duplicated = dup(0);
    if (duplicated != 64) return 23;
    close(duplicated);
    return 0;
  };
  pid_t pid = clone(child_fn, stack.data() + stack.size(), CLONE_FILES | SIGCHLD, &args);
  if (pid < 0) return 24;
  int status = 0;
  if (waitpid(pid, &status, 0) != pid || !WIFEXITED(status)) return 25;
  return WEXITSTATUS(status);
}

TEST(CloseRangeSemantics, SparseHighFdAndNextFd) {
  FdGuard base = OpenNull();
  ASSERT_GE(base.get(), 0);
  FdGuard high(dup2(base.get(), 1000));
  ASSERT_EQ(1000, high.get());

  SharedChildArgs args = {3, UINT_MAX, CLOSE_RANGE_UNSHARE | CLOSE_RANGE_CLOEXEC,
                          high.get(), base.get(), false, true};
  ASSERT_EQ(0, RunCloneFilesChild(&args));
  EXPECT_FALSE(HasCloexec(base.get()));
  EXPECT_FALSE(HasCloexec(high.get()));

  pid_t isolated = fork();
  ASSERT_GE(isolated, 0);
  if (isolated == 0) _exit(SparseNextFdProcess());
  int status = 0;
  ASSERT_EQ(isolated, waitpid(isolated, &status, 0));
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(CloseRangeSemantics, CloexecIgnoresLoweredRlimitForExistingFds) {
  pid_t child = fork();
  ASSERT_GE(child, 0);
  if (child == 0) {
    std::vector<int> fds;
    for (int i = 0; i < 40; ++i) {
      int fd = open("/dev/null", O_RDWR);
      if (fd < 0) _exit(30);
      fds.push_back(fd);
    }
    rlimit limit{};
    if (getrlimit(RLIMIT_NOFILE, &limit) != 0) _exit(31);
    limit.rlim_cur = 25;
    if (setrlimit(RLIMIT_NOFILE, &limit) != 0) _exit(32);
    int plain_high = fds[fds.size() - 2];
    int unshare_high = fds.back();
    if (CloseRange(plain_high, plain_high, CLOSE_RANGE_CLOEXEC) != 0) _exit(33);
    if (!HasCloexec(plain_high)) _exit(34);

    SharedChildArgs args = {static_cast<unsigned int>(unshare_high),
                            static_cast<unsigned int>(unshare_high),
                            CLOSE_RANGE_CLOEXEC | CLOSE_RANGE_UNSHARE,
                            unshare_high, plain_high, false, true};
    if (RunCloneFilesChild(&args) != 0) _exit(35);
    if (HasCloexec(unshare_high)) _exit(36);
    _exit(0);
  }
  int status = 0;
  ASSERT_EQ(child, waitpid(child, &status, 0));
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(CloseRangeSemantics, UnshareOnPrivateTablePreservesPosixLockOwner) {
  char path[] = "/tmp/close-range-lock-XXXXXX";
  FdGuard file(mkstemp(path));
  ASSERT_GE(file.get(), 0);
  ASSERT_EQ(0, unlink(path));

  flock lock{};
  lock.l_type = F_WRLCK;
  lock.l_whence = SEEK_SET;
  lock.l_start = 0;
  lock.l_len = 0;
  ASSERT_EQ(0, fcntl(file.get(), F_SETLK, &lock));
  ASSERT_EQ(0, CloseRange(UINT_MAX, UINT_MAX, CLOSE_RANGE_UNSHARE));

  pid_t child = fork();
  ASSERT_GE(child, 0);
  if (child == 0) {
    // The inherited fd uses a new fork files-table owner. A conflicting lock
    // still proves the parent's original owner was not released by close_range.
    flock conflict = lock;
    int rc = fcntl(file.get(), F_SETLK, &conflict);
    _exit(rc == -1 && (errno == EACCES || errno == EAGAIN) ? 0 : 40);
  }
  int status = 0;
  ASSERT_EQ(child, waitpid(child, &status, 0));
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
}

}  // namespace

int main(int argc, char** argv) {
  testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
