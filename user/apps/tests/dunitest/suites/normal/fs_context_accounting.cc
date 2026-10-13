#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include <string>

namespace {

long long NowMs() {
  timespec now{};
  clock_gettime(CLOCK_MONOTONIC, &now);
  return now.tv_sec * 1000LL + now.tv_nsec / 1000000;
}

class ContextGuard {
 public:
  ContextGuard() : cwd_(open(".", O_RDONLY | O_DIRECTORY | O_CLOEXEC)) {
    mask_ = umask(0);
    umask(mask_);
  }
  ~ContextGuard() {
    if (cwd_ >= 0) {
      fchdir(cwd_);
      close(cwd_);
    }
    umask(mask_);
    if (!directory_.empty()) rmdir(directory_.c_str());
  }
  bool Prepare() {
    if (cwd_ < 0) return false;
    char path[] = "/tmp/fs-context-accounting-XXXXXX";
    char* created = mkdtemp(path);
    if (created == nullptr) return false;
    directory_ = created;
    return true;
  }
  const char* directory() const { return directory_.c_str(); }

 private:
  int cwd_;
  mode_t mask_;
  std::string directory_;
};

struct ChildArgs {
  const char* directory;
  bool unshare;
  bool exec;
  bool unreachable = false;
};

int ChangeContext(void* opaque) {
  auto* args = static_cast<ChildArgs*>(opaque);
  if (setpgid(0, 0) != 0) return 2;
  if (args->unreachable) {
    char before[4096];
    if (getcwd(before, sizeof(before)) == nullptr) return 7;
    if (chroot(args->directory) != 0) return errno == EPERM ? 77 : 8;
    // Raw Linux getcwd reports an unreachable path. libc deliberately rejects
    // that result, so this oracle must not call its getcwd wrapper here.
    char after[4096];
    if (syscall(SYS_getcwd, after, sizeof(after)) < 0) return 9;
    const std::string expected = std::string("(unreachable)") + before;
    return expected == after ? 0 : 10;
  }
  if (args->unshare && unshare(CLONE_FS) != 0) return errno == EPERM ? 77 : 3;
  if (args->exec) {
    execl("/proc/self/exe", "/proc/self/exe", "--fs-context-worker", args->directory,
          static_cast<char*>(nullptr));
    return 4;
  }
  umask(0027);
  return chdir(args->directory) == 0 ? 0 : 5;
}

class Child {
 public:
  ~Child() {
    if (pid_ > 0) {
      kill(-pid_, SIGKILL);
      kill(pid_, SIGKILL);
      Wait();
    }
    if (stack_ != MAP_FAILED) munmap(stack_, kStackSize);
  }
  bool Start(ChildArgs* args, bool shared) {
    stack_ = mmap(nullptr, kStackSize, PROT_READ | PROT_WRITE,
                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (stack_ == MAP_FAILED) return false;
    pid_ = clone(ChangeContext, static_cast<char*>(stack_) + kStackSize,
                 SIGCHLD | (shared ? CLONE_FS : 0), args);
    if (pid_ > 0) setpgid(pid_, pid_);
    return pid_ > 0;
  }
  bool Wait() {
    const long long deadline = NowMs() + 5000;
    while (pid_ > 0 && NowMs() < deadline) {
      int status = 0;
      const pid_t result = waitpid(pid_, &status, WNOHANG);
      if (result == pid_) {
        pid_ = -1;
        status_ = status;
        return true;
      }
      if (result < 0 && errno != EINTR) {
        if (errno == ECHILD) pid_ = -1;  // No longer own a child PID to signal.
        return false;
      }
      timespec delay{0, 1000000};
      nanosleep(&delay, nullptr);
    }
    return false;
  }
  int exit_code() const { return WIFEXITED(status_) ? WEXITSTATUS(status_) : -1; }

 private:
  static constexpr size_t kStackSize = 64 * 1024;
  void* stack_ = MAP_FAILED;
  pid_t pid_ = -1;
  int status_ = 0;
};

void CheckContext(bool shared, bool do_unshare, bool do_exec) {
  ContextGuard guard;
  ASSERT_TRUE(guard.Prepare()) << strerror(errno);
  char before[4096];
  ASSERT_NE(nullptr, getcwd(before, sizeof(before)));
  umask(0077);
  ChildArgs args{guard.directory(), do_unshare, do_exec};
  Child child;
  ASSERT_TRUE(child.Start(&args, shared)) << strerror(errno);
  ASSERT_TRUE(child.Wait()) << "child did not complete within five seconds";
  if (do_unshare && child.exit_code() == 77) GTEST_SKIP() << "CLONE_FS unshare denied";
  ASSERT_EQ(0, child.exit_code());
  char after[4096];
  ASSERT_NE(nullptr, getcwd(after, sizeof(after)));
  const mode_t actual = umask(0077);
  const bool should_share = shared && !do_unshare;
  EXPECT_EQ(should_share ? 0027U : 0077U, actual);
  EXPECT_STREQ(should_share ? guard.directory() : before, after);
}

TEST(FsContextAccounting, SharedCloneUsesTheOriginalContext) { CheckContext(true, false, false); }
TEST(FsContextAccounting, PrivateCloneIsolatesContext) { CheckContext(false, false, false); }
TEST(FsContextAccounting, UnshareDetachesOneTask) { CheckContext(true, true, false); }
TEST(FsContextAccounting, ExecDoesNotInventAnFsUnshare) { CheckContext(true, false, true); }

TEST(FsContextAccounting, CwdTracksCanonicalSymlinkAndRename) {
  ContextGuard guard;
  ASSERT_TRUE(guard.Prepare());
  const std::string old_path = std::string(guard.directory()) + "/before";
  const std::string new_path = std::string(guard.directory()) + "/after";
  const std::string link_path = std::string(guard.directory()) + "/alias";
  struct Cleanup {
    const std::string& old_path;
    const std::string& new_path;
    const std::string& link_path;
    ~Cleanup() {
      unlink(link_path.c_str());
      rmdir(old_path.c_str());
      rmdir(new_path.c_str());
    }
  } cleanup{old_path, new_path, link_path};
  ASSERT_EQ(0, mkdir(old_path.c_str(), 0700));
  ASSERT_EQ(0, symlink("before", link_path.c_str()));
  ASSERT_EQ(0, chdir(link_path.c_str()));
  char cwd[4096];
  ASSERT_NE(nullptr, getcwd(cwd, sizeof(cwd)));
  EXPECT_EQ(old_path, cwd);
  ASSERT_EQ(0, rename(old_path.c_str(), new_path.c_str()));
  ASSERT_NE(nullptr, getcwd(cwd, sizeof(cwd)));
  EXPECT_EQ(new_path, cwd);
}

TEST(FsContextAccounting, DeletedDirectoryFdRemainsUsableButCwdIsUnlinked) {
  ContextGuard guard;
  ASSERT_TRUE(guard.Prepare());
  const std::string path = std::string(guard.directory()) + "/removed";
  ASSERT_EQ(0, mkdir(path.c_str(), 0700));
  struct Cleanup {
    std::string path;
    int fd;
    ~Cleanup() {
      if (fd >= 0) close(fd);
      rmdir(path.c_str());
    }
  } cleanup{path, open(path.c_str(), O_RDONLY | O_DIRECTORY | O_CLOEXEC)};
  ASSERT_GE(cleanup.fd, 0);
  ASSERT_EQ(0, rmdir(path.c_str()));
  ASSERT_EQ(0, fchdir(cleanup.fd));
  char cwd[4096];
  errno = 0;
  EXPECT_EQ(-1, syscall(SYS_getcwd, cwd, sizeof(cwd)));
  EXPECT_EQ(ENOENT, errno);
}

TEST(FsContextAccounting, RawCwdReportsRangeAndProtectedCopyErrors) {
  ContextGuard guard;
  ASSERT_TRUE(guard.Prepare());
  ASSERT_EQ(0, chdir(guard.directory()));
  char cwd[4096];
  const long size = syscall(SYS_getcwd, cwd, sizeof(cwd));
  ASSERT_GT(size, 1);
  EXPECT_EQ(static_cast<long>(strlen(cwd) + 1), size);
  errno = 0;
  EXPECT_EQ(-1, syscall(SYS_getcwd, cwd, size - 1));
  EXPECT_EQ(ERANGE, errno);
  errno = 0;
  EXPECT_EQ(-1, syscall(SYS_getcwd, cwd, 0));
  EXPECT_EQ(ERANGE, errno);
  const long page_size = sysconf(_SC_PAGESIZE);
  ASSERT_GT(page_size, size);
  void* mapping = mmap(nullptr, 2 * page_size, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  ASSERT_NE(MAP_FAILED, mapping);
  struct MappingCleanup {
    void* mapping;
    size_t size;
    ~MappingCleanup() { munmap(mapping, size); }
  } cleanup{mapping, static_cast<size_t>(2 * page_size)};
  ASSERT_EQ(0, mprotect(static_cast<char*>(mapping) + page_size, page_size, PROT_NONE));
  errno = 0;
  EXPECT_EQ(-1, syscall(SYS_getcwd, static_cast<char*>(mapping) + page_size - 1, size));
  EXPECT_EQ(EFAULT, errno);
}

TEST(FsContextAccounting, ValidDeepCwdIsLimitedByBytesNotComponentCount) {
  fprintf(stderr, "[cwd-deep] preparing\n");
  ContextGuard guard;
  ASSERT_TRUE(guard.Prepare());
  struct TreeCleanup {
    int current;
    int depth = 0;
    ~TreeCleanup() {
      fprintf(stderr, "[cwd-deep] removing %d entries\n", depth);
      while (current >= 0 && depth-- > 0) {
        const int parent = openat(current, "..", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
        close(current);
        current = parent;
        if (parent >= 0) unlinkat(parent, "d", AT_REMOVEDIR);
      }
      if (current >= 0) close(current);
      fprintf(stderr, "[cwd-deep] tree cleanup complete\n");
    }
  } cleanup{open(guard.directory(), O_RDONLY | O_DIRECTORY | O_CLOEXEC)};
  // More than the old absolute walker's 1024-component heuristic, but well
  // within Linux's byte-oriented PATH_MAX (including the trailing NUL).
  constexpr int kDepth = 1100;
  ASSERT_GE(cleanup.current, 0);
  std::string expected = guard.directory();
  for (int depth = 0; depth < kDepth; ++depth) {
    ASSERT_EQ(0, mkdirat(cleanup.current, "d", 0700)) << depth;
    const int child = openat(cleanup.current, "d", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (child < 0) unlinkat(cleanup.current, "d", AT_REMOVEDIR);
    ASSERT_GE(child, 0) << depth;
    close(cleanup.current);
    cleanup.current = child;
    ++cleanup.depth;
    expected += "/d";
  }
  fprintf(stderr, "[cwd-deep] created 1100 entries\n");
  ASSERT_EQ(0, fchdir(cleanup.current));
  fprintf(stderr, "[cwd-deep] cwd installed\n");
  char cwd[4096];
  ASSERT_NE(nullptr, getcwd(cwd, sizeof(cwd))) << strerror(errno);
  EXPECT_EQ(expected, cwd);
  fprintf(stderr, "[cwd-deep] path verified\n");
}

TEST(FsContextAccounting, RawCwdMarksAPathOutsideChrootUnreachable) {
  ContextGuard guard;
  ASSERT_TRUE(guard.Prepare());
  ChildArgs args{guard.directory(), false, false, true};
  Child child;
  ASSERT_TRUE(child.Start(&args, false)) << strerror(errno);
  ASSERT_TRUE(child.Wait()) << "unreachable-cwd child exceeded five seconds";
  if (child.exit_code() == 77) GTEST_SKIP() << "chroot requires CAP_SYS_CHROOT";
  EXPECT_EQ(0, child.exit_code());
}

}  // namespace

int main(int argc, char** argv) {
  if (argc == 3 && strcmp(argv[1], "--fs-context-worker") == 0) {
    umask(0027);
    return chdir(argv[2]) == 0 ? 0 : 6;
  }
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
