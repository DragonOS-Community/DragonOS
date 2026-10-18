#include <gtest/gtest.h>

#include <fcntl.h>
#include <signal.h>
#include <sys/mount.h>
#include <sys/resource.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

#include <cerrno>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>

namespace {

enum class Api { Write, Writev, Pwrite };

class DebugfsWritePolicy : public ::testing::TestWithParam<Api> {
 protected:
  void SetUp() override {
    ASSERT_NE(mkdtemp(directory_), nullptr) << strerror(errno);
    ASSERT_EQ(mount("none", directory_, "debugfs", 0, nullptr), 0) << strerror(errno);
    mounted_ = true;
    path_ = std::string(directory_) + "/fuse/stats_mode";
    original_ = ReadMode();
    ASSERT_TRUE(original_ == "off\n" || original_ == "light\n" || original_ == "detailed\n")
        << "unexpected original mode: " << original_;
    restore_ = true;
    ASSERT_TRUE(SetMode("off\n"));
    ASSERT_EQ(ReadMode(), "off\n");
  }

  void TearDown() override {
    // TearDown runs after fatal assertions too. Restore through a fresh fd so
    // a rejected command's cursor cannot prevent restoration of global state.
    if (restore_) {
      EXPECT_TRUE(SetMode(original_));
      EXPECT_EQ(ReadMode(), original_);
    }
    if (mounted_) {
      EXPECT_EQ(umount2(directory_, MNT_DETACH), 0);
    }
    EXPECT_EQ(rmdir(directory_), 0);
  }

  std::string ReadMode() {
    const int fd = open(path_.c_str(), O_RDONLY | O_CLOEXEC);
    if (fd < 0) return {};
    char bytes[32];
    const ssize_t n = read(fd, bytes, sizeof(bytes));
    close(fd);
    return n > 0 ? std::string(bytes, n) : std::string();
  }

  bool SetMode(const std::string& mode) {
    const int fd = open(path_.c_str(), O_WRONLY | O_CLOEXEC);
    if (fd < 0) return false;
    const ssize_t result = write(fd, mode.data(), mode.size());
    close(fd);
    return result == static_cast<ssize_t>(mode.size());
  }

  ssize_t Submit(int fd, const std::string& command, off_t offset) {
    iovec vectors[] = {{const_cast<char*>(command.data()), 2},
                       {const_cast<char*>(command.data()) + 2, command.size() - 2}};
    switch (GetParam()) {
      case Api::Write: return write(fd, command.data(), command.size());
      case Api::Writev: return writev(fd, vectors, 2);
      case Api::Pwrite: return pwrite(fd, command.data(), command.size(), offset);
    }
    return -1;
  }

  std::string LongPrefix() {
    std::string command = "light";
    command.resize(static_cast<size_t>(sysconf(_SC_PAGESIZE)), ' ');
    return command;
  }

  char directory_[64] = "/tmp/dunit-debugfs-write-XXXXXX";
  std::string path_, original_;
  bool mounted_ = false;
  bool restore_ = false;
};

TEST_P(DebugfsWritePolicy, CompleteLongCommandReachesItsOwner) {
  std::string command = LongPrefix() + "     \n";
  const int fd = open(path_.c_str(), O_WRONLY | O_CLOEXEC);
  ASSERT_GE(fd, 0);
  EXPECT_EQ(Submit(fd, command, 0), static_cast<ssize_t>(command.size())) << strerror(errno);
  EXPECT_EQ(lseek(fd, 0, SEEK_CUR), GetParam() == Api::Pwrite ? 0 :
            static_cast<off_t>(command.size()));
  EXPECT_EQ(close(fd), 0);
  EXPECT_EQ(ReadMode(), "light\n");
}

TEST_P(DebugfsWritePolicy, InvalidSuffixBeyondPageCannotCommitValidPrefix) {
  const std::string command = LongPrefix() + "bogus";
  const int fd = open(path_.c_str(), O_WRONLY | O_CLOEXEC);
  ASSERT_GE(fd, 0);
  errno = 0;
  EXPECT_EQ(Submit(fd, command, 0), -1);
  EXPECT_EQ(errno, EINVAL);
  EXPECT_EQ(lseek(fd, 0, SEEK_CUR), 0);
  EXPECT_EQ(close(fd), 0);
  EXPECT_EQ(ReadMode(), "off\n");
}

TEST_P(DebugfsWritePolicy, OwnerRejectsNonzeroOffsetWithoutMutation) {
  const int fd = open(path_.c_str(), O_WRONLY | O_CLOEXEC);
  ASSERT_GE(fd, 0);
  ASSERT_EQ(lseek(fd, 7, SEEK_SET), 7);
  errno = 0;
  EXPECT_EQ(Submit(fd, "light\n", 7), -1);
  EXPECT_EQ(errno, EINVAL);
  EXPECT_EQ(lseek(fd, 0, SEEK_CUR), 7);
  EXPECT_EQ(close(fd), 0);
  EXPECT_EQ(ReadMode(), "off\n");
}

TEST_P(DebugfsWritePolicy, AppendKeepsExistingSyntheticEofPolicy) {
  const int fd = open(path_.c_str(), O_WRONLY | O_APPEND | O_CLOEXEC);
  ASSERT_GE(fd, 0);
  errno = 0;
  EXPECT_EQ(Submit(fd, "light\n", 0), -1);
  EXPECT_EQ(errno, EINVAL);
  EXPECT_EQ(lseek(fd, 0, SEEK_CUR), 0);
  EXPECT_EQ(close(fd), 0);
  EXPECT_EQ(ReadMode(), "off\n");
}

TEST_P(DebugfsWritePolicy, FileSizePolicyIsNotChangedBySysfsFix) {
  const pid_t child = fork();
  ASSERT_GE(child, 0);
  if (child == 0) {
    alarm(10);
    if (signal(SIGXFSZ, SIG_IGN) == SIG_ERR) _exit(10);
    rlimit limit{};
    if (getrlimit(RLIMIT_FSIZE, &limit) != 0) _exit(11);
    limit.rlim_cur = 0;
    if (setrlimit(RLIMIT_FSIZE, &limit) != 0) _exit(12);
    const int fd = open(path_.c_str(), O_WRONLY | O_CLOEXEC);
    if (fd < 0) _exit(13);
    errno = 0;
    const ssize_t result = Submit(fd, "light\n", 0);
    const int error = errno;
    const off_t position = lseek(fd, 0, SEEK_CUR);
    close(fd);
    if (result != -1 || error != EFBIG || position != 0) {
      dprintf(STDERR_FILENO, "debugfs FSIZE result=%zd errno=%d position=%lld\n",
              result, error, static_cast<long long>(position));
      _exit(14);
    }
    _exit(0);
  }
  int status = 0;
  pid_t result;
  do { result = waitpid(child, &status, 0); } while (result < 0 && errno == EINTR);
  ASSERT_EQ(result, child);
  ASSERT_TRUE(WIFEXITED(status)) << status;
  EXPECT_EQ(WEXITSTATUS(status), 0);
  EXPECT_EQ(ReadMode(), "off\n");
}

INSTANTIATE_TEST_SUITE_P(
    Interfaces, DebugfsWritePolicy,
    ::testing::Values(Api::Write, Api::Writev, Api::Pwrite),
    [](const ::testing::TestParamInfo<Api>& info) {
      switch (info.param) {
        case Api::Write: return "Write";
        case Api::Writev: return "Writev";
        case Api::Pwrite: return "Pwrite";
      }
      return "Unknown";
    });

}  // namespace

int main(int argc, char** argv) {
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
