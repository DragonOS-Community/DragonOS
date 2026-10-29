#include <gtest/gtest.h>

#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

#include <cerrno>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <functional>
#include <string>

#ifndef RWF_DSYNC
#define RWF_DSYNC 0x00000002
#endif
#ifndef RWF_SYNC
#define RWF_SYNC 0x00000004
#endif
#ifndef RWF_NOWAIT
#define RWF_NOWAIT 0x00000008
#endif
#ifndef RWF_APPEND
#define RWF_APPEND 0x00000010
#endif

namespace {

volatile sig_atomic_t size_signals = 0;
void OnSizeSignal(int) { size_signals = 1; }

class SysfsView {
 public:
  bool Initialize() {
    // A fresh sysfs mount projects the child's own network namespace.
    if (unshare(CLONE_NEWNET | CLONE_NEWNS) != 0 ||
        mount(nullptr, "/", nullptr, MS_REC | MS_PRIVATE, nullptr) != 0 ||
        mkdtemp(path_) == nullptr) {
      return false;
    }
    mounted_ = mount("sysfs", path_, "sysfs", MS_NOSUID | MS_NODEV | MS_NOEXEC,
                     nullptr) == 0;
    return mounted_;
  }
  std::string Attribute() const { return std::string(path_) + "/class/net/lo/mtu"; }
  ~SysfsView() {
    if (mounted_) umount2(path_, MNT_DETACH);
    rmdir(path_);
  }
 private:
  char path_[64] = "/tmp/dunit-sysfs-write-XXXXXX";
  bool mounted_ = false;
};

class Fd {
 public:
  explicit Fd(int value) : value_(value) {}
  ~Fd() { if (value_ >= 0) close(value_); }
  int Get() const { return value_; }
 private:
  int value_;
};

std::string ReadValue(const std::string& path) {
  Fd fd(open(path.c_str(), O_RDONLY | O_CLOEXEC));
  char bytes[64];
  const ssize_t n = read(fd.Get(), bytes, sizeof(bytes));
  return n > 0 ? std::string(bytes, n) : std::string();
}

void RunChild(const std::function<void()>& body) {
  const pid_t pid = fork();
  ASSERT_GE(pid, 0);
  if (pid == 0) {
    alarm(15);
    body();
    std::fflush(nullptr);
    _exit(::testing::Test::HasFailure() ? 1 : 0);
  }
  int status = 0;
  ASSERT_EQ(waitpid(pid, &status, 0), pid);
  ASSERT_TRUE(WIFEXITED(status)) << "status=" << status;
  EXPECT_EQ(WEXITSTATUS(status), 0);
}

enum class Api { Write, Writev, Pwrite, Pwritev, Pwritev2 };

bool Positional(Api api) {
  return api == Api::Pwrite || api == Api::Pwritev || api == Api::Pwritev2;
}

ssize_t Write(Api api, int fd, const char* bytes, size_t count, off_t offset = 7) {
  // Split vectors inside the selected prefix, never at the fault boundary.
  const size_t first = count > 1 ? 1 : count;
  iovec vec[2] = {{const_cast<char*>(bytes), first},
                  {const_cast<char*>(bytes + first), count - first}};
  switch (api) {
    case Api::Write: return write(fd, bytes, count);
    case Api::Writev: return writev(fd, vec, 2);
    case Api::Pwrite: return pwrite(fd, bytes, count, offset);
    case Api::Pwritev: return pwritev(fd, vec, 2, offset);
    case Api::Pwritev2: return pwritev2(fd, vec, 2, offset, 0);
  }
  return -1;
}

class SysfsWritePolicy : public ::testing::TestWithParam<Api> {};

TEST_P(SysfsWritePolicy, FileSizeLimitDoesNotBlockControlWrite) {
  const Api api = GetParam();
  RunChild([api] {
    SysfsView view;
    ASSERT_TRUE(view.Initialize()) << strerror(errno);
    Fd fd(open(view.Attribute().c_str(), O_WRONLY | O_CLOEXEC));
    ASSERT_GE(fd.Get(), 0);
    ASSERT_EQ(lseek(fd.Get(), 7, SEEK_SET), 7);
    size_signals = 0;
    ASSERT_NE(signal(SIGXFSZ, OnSizeSignal), SIG_ERR);
    rlimit limit{0, 0};
    ASSERT_EQ(setrlimit(RLIMIT_FSIZE, &limit), 0);
    constexpr char value[] = "1400\n";
    EXPECT_EQ(Write(api, fd.Get(), value, sizeof(value) - 1), 5) << strerror(errno);
    EXPECT_EQ(ReadValue(view.Attribute()), value);
    EXPECT_EQ(size_signals, 0);
    EXPECT_EQ(lseek(fd.Get(), 0, SEEK_CUR), Positional(api) ? 7 : 12);
  });
}

TEST_P(SysfsWritePolicy, AppendPreservesCallbackPosition) {
  const Api api = GetParam();
  RunChild([api] {
    SysfsView view;
    ASSERT_TRUE(view.Initialize()) << strerror(errno);
    Fd fd(open(view.Attribute().c_str(), O_WRONLY | O_APPEND | O_CLOEXEC));
    ASSERT_GE(fd.Get(), 0);
    ASSERT_EQ(lseek(fd.Get(), 7, SEEK_SET), 7);
    constexpr char value[] = "1400\n";
    ASSERT_EQ(Write(api, fd.Get(), value, sizeof(value) - 1), 5) << strerror(errno);
    EXPECT_EQ(ReadValue(view.Attribute()), value);
    EXPECT_EQ(size_signals, 0);
    EXPECT_EQ(lseek(fd.Get(), 0, SEEK_CUR), Positional(api) ? 7 : 12);
  });
}

TEST_P(SysfsWritePolicy, PageLimitPrecedesAccessToUnselectedTail) {
  const Api api = GetParam();
  RunChild([api] {
    SysfsView view;
    ASSERT_TRUE(view.Initialize()) << strerror(errno);
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    char* mapping = static_cast<char*>(mmap(nullptr, page * 2, PROT_READ | PROT_WRITE,
                                          MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(mapping, MAP_FAILED);
    // Linux kstrtoul and DragonOS parse_mtu stop at embedded NUL; no padding
    // becomes part of the numeric value. The selected page is still fully copied.
    std::memcpy(mapping, "1400\n", 5);
    ASSERT_EQ(mprotect(mapping + page, page, PROT_NONE), 0);
    Fd fd(open(view.Attribute().c_str(), O_WRONLY | O_CLOEXEC));
    ASSERT_GE(fd.Get(), 0);
    ASSERT_EQ(lseek(fd.Get(), 7, SEEK_SET), 7);
    EXPECT_EQ(Write(api, fd.Get(), mapping, page * 2), static_cast<ssize_t>(page))
        << strerror(errno);
    EXPECT_EQ(ReadValue(view.Attribute()), "1400\n");
    EXPECT_EQ(lseek(fd.Get(), 0, SEEK_CUR),
              Positional(api) ? 7 : static_cast<off_t>(page + 7));
    EXPECT_EQ(munmap(mapping, page * 2), 0);
  });
}

TEST_P(SysfsWritePolicy, IncompleteSelectedPrefixDoesNotCommit) {
  const Api api = GetParam();
  RunChild([api] {
    SysfsView view;
    ASSERT_TRUE(view.Initialize()) << strerror(errno);
    const std::string original = ReadValue(view.Attribute());
    ASSERT_FALSE(original.empty());
    ASSERT_NE(original, "1400\n");
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    char* mapping = static_cast<char*>(mmap(nullptr, page * 2, PROT_READ | PROT_WRITE,
                                          MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(mapping, MAP_FAILED);
    char* bytes = mapping + page - 16;
    std::memcpy(bytes, "1400\n", 5);
    ASSERT_EQ(mprotect(mapping + page, page, PROT_NONE), 0);
    Fd fd(open(view.Attribute().c_str(), O_WRONLY | O_CLOEXEC));
    ASSERT_GE(fd.Get(), 0);
    ASSERT_EQ(lseek(fd.Get(), 7, SEEK_SET), 7);
    errno = 0;
    EXPECT_EQ(Write(api, fd.Get(), bytes, 32), -1);
    EXPECT_EQ(errno, EFAULT);
    EXPECT_EQ(ReadValue(view.Attribute()), original);
    EXPECT_EQ(lseek(fd.Get(), 0, SEEK_CUR), 7);
    EXPECT_EQ(munmap(mapping, page * 2), 0);
  });
}

TEST_P(SysfsWritePolicy, EntireFaultDoesNotCommit) {
  const Api api = GetParam();
  RunChild([api] {
    SysfsView view;
    ASSERT_TRUE(view.Initialize()) << strerror(errno);
    const std::string original = ReadValue(view.Attribute());
    ASSERT_FALSE(original.empty());
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    char* bytes = static_cast<char*>(mmap(nullptr, page, PROT_NONE,
                                        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(bytes, MAP_FAILED);
    Fd fd(open(view.Attribute().c_str(), O_WRONLY | O_CLOEXEC));
    ASSERT_GE(fd.Get(), 0);
    ASSERT_EQ(lseek(fd.Get(), 7, SEEK_SET), 7);
    errno = 0;
    EXPECT_EQ(Write(api, fd.Get(), bytes, 5), -1);
    EXPECT_EQ(errno, EFAULT);
    EXPECT_EQ(ReadValue(view.Attribute()), original);
    EXPECT_EQ(lseek(fd.Get(), 0, SEEK_CUR), 7);
    EXPECT_EQ(munmap(bytes, page), 0);
  });
}

INSTANTIATE_TEST_SUITE_P(
    Interfaces, SysfsWritePolicy,
    ::testing::Values(Api::Write, Api::Writev, Api::Pwrite, Api::Pwritev, Api::Pwritev2),
    [](const ::testing::TestParamInfo<Api>& info) {
      switch (info.param) {
        case Api::Write: return "Write";
        case Api::Writev: return "Writev";
        case Api::Pwrite: return "Pwrite";
        case Api::Pwritev: return "Pwritev";
        case Api::Pwritev2: return "Pwritev2";
      }
      return "Unknown";
    });

TEST(SysfsWriteFlags, SyncAndAppendKeepControlPolicy) {
  RunChild([] {
    SysfsView view;
    ASSERT_TRUE(view.Initialize()) << strerror(errno);
    Fd fd(open(view.Attribute().c_str(), O_WRONLY | O_CLOEXEC));
    ASSERT_GE(fd.Get(), 0);
    ASSERT_EQ(lseek(fd.Get(), 7, SEEK_SET), 7);
    constexpr char value[] = "1400\n";
    iovec vec{const_cast<char*>(value), sizeof(value) - 1};
    rlimit limit{0, 0};
    size_signals = 0;
    ASSERT_NE(signal(SIGXFSZ, OnSizeSignal), SIG_ERR);
    ASSERT_EQ(setrlimit(RLIMIT_FSIZE, &limit), 0);
    for (const int flags : {RWF_DSYNC, RWF_SYNC, RWF_APPEND,
                            RWF_SYNC | RWF_APPEND}) {
      EXPECT_EQ(pwritev2(fd.Get(), &vec, 1, 11, flags), 5)
          << "flags=" << flags << " errno=" << errno;
      EXPECT_EQ(ReadValue(view.Attribute()), value);
    EXPECT_EQ(size_signals, 0);
      EXPECT_EQ(lseek(fd.Get(), 0, SEEK_CUR), 7);
    }
  });
}

TEST(SysfsWriteFlags, UnsupportedFlagsFailBeforeAccessOrMutation) {
  RunChild([] {
    SysfsView view;
    ASSERT_TRUE(view.Initialize()) << strerror(errno);
    const std::string original = ReadValue(view.Attribute());
    Fd fd(open(view.Attribute().c_str(), O_WRONLY | O_CLOEXEC));
    ASSERT_GE(fd.Get(), 0);
    ASSERT_EQ(lseek(fd.Get(), 7, SEEK_SET), 7);
    void* inaccessible = mmap(nullptr, 4096, PROT_NONE,
                              MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(inaccessible, MAP_FAILED);
    iovec vec{inaccessible, 5};
    for (const int flags : {RWF_NOWAIT, 0x40000000}) {
      errno = 0;
      EXPECT_EQ(pwritev2(fd.Get(), &vec, 1, 11, flags), -1);
      EXPECT_EQ(errno, EOPNOTSUPP) << "flags=" << flags;
      EXPECT_EQ(ReadValue(view.Attribute()), original);
      EXPECT_EQ(lseek(fd.Get(), 0, SEEK_CUR), 7);
    }
    EXPECT_EQ(munmap(inaccessible, 4096), 0);
  });
}

}  // namespace

int main(int argc, char** argv) {
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
