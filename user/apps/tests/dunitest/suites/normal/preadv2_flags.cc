#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <signal.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

#include <string>

namespace {

constexpr unsigned kHipri = 0x01;
constexpr unsigned kDsync = 0x02;
constexpr unsigned kSync = 0x04;
constexpr unsigned kNowait = 0x08;
constexpr unsigned kAppend = 0x10;
constexpr unsigned kUnknown = 0x80000000U;

// The native Linux ABI uses six arguments on all supported 64-bit targets.
// pos_l contains the entire offset; pos_h is ignored on a 64-bit kernel.
ssize_t Preadv2(int fd, const iovec* vectors, size_t count, int64_t offset,
               unsigned long flags, unsigned long high = 0) {
  return syscall(SYS_preadv2, fd, vectors, count, static_cast<uint64_t>(offset), high, flags);
}

class Preadv2Flags : public testing::Test {
 protected:
  void SetUp() override {
    char name[] = "/tmp/preadv2-flags-XXXXXX";
    fd_ = mkstemp(name);
    ASSERT_GE(fd_, 0) << strerror(errno);
    path_ = name;
    ASSERT_EQ(6, write(fd_, "abcdef", 6));
    ASSERT_EQ(0, lseek(fd_, 0, SEEK_SET));
    bad_ = mmap(nullptr, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, bad_);
  }
  void TearDown() override {
    if (bad_ != MAP_FAILED) munmap(bad_, 4096);
    if (fd_ >= 0) close(fd_);
    if (!path_.empty()) unlink(path_.c_str());
  }
  void ExpectError(int error, int fd, const iovec* vectors, size_t count,
                   int64_t offset, unsigned long flags) {
    errno = 0;
    EXPECT_EQ(-1, Preadv2(fd, vectors, count, offset, flags));
    EXPECT_EQ(error, errno);
  }
  int fd_ = -1;
  std::string path_;
  void* bad_ = MAP_FAILED;
};

TEST_F(Preadv2Flags, NegativeOffsetPrecedesDescriptorImportAndFlags) {
  ExpectError(EINVAL, -1, static_cast<iovec*>(bad_), 1, -2, kUnknown);
}

TEST_F(Preadv2Flags, DescriptorLookupPrecedesImportAndFlags) {
  for (int64_t offset : {int64_t(-1), int64_t(0)}) {
    ExpectError(EBADF, -1, static_cast<iovec*>(bad_), 1, offset, kUnknown);
  }
}

TEST_F(Preadv2Flags, VectorImportPrecedesFlagsAndReadMode) {
  const int write_only = open(path_.c_str(), O_WRONLY);
  ASSERT_GE(write_only, 0);
  for (int64_t offset : {int64_t(-1), int64_t(0)}) {
    ExpectError(EFAULT, fd_, static_cast<iovec*>(bad_), 1, offset, kUnknown);
    ExpectError(EINVAL, fd_, nullptr, 1025, offset, kUnknown);
    // Linux 6.6 imports before checking FMODE_READ. Linux 6.8 moved the
    // mode check earlier; this intentionally retains the 6.6 ABI target.
    ExpectError(EFAULT, write_only, static_cast<iovec*>(bad_), 1, offset, kUnknown);
  }
  close(write_only);
}

TEST_F(Preadv2Flags, ReadModePrecedesFlagsAndEmptyVectorSuccess) {
  const int write_only = open(path_.c_str(), O_WRONLY);
  ASSERT_GE(write_only, 0);
  char byte;
  iovec vector = {&byte, 1};
  for (int64_t offset : {int64_t(-1), int64_t(0)}) {
    ExpectError(EBADF, write_only, &vector, 1, offset, kUnknown);
    ExpectError(EBADF, write_only, nullptr, 0, offset, kUnknown);
  }
  close(write_only);
}

TEST_F(Preadv2Flags, ZeroBytesSkipFlagValidationButStillImportVectors) {
  iovec empty = {bad_, 0};
  for (int64_t offset : {int64_t(-1), int64_t(0)}) {
    EXPECT_EQ(0, Preadv2(fd_, nullptr, 0, offset, kUnknown | kNowait));
    EXPECT_EQ(0, Preadv2(fd_, &empty, 1, offset, kUnknown | kNowait));
    ExpectError(EFAULT, fd_, static_cast<iovec*>(bad_), 1, offset, kUnknown);
  }
  EXPECT_EQ(0, lseek(fd_, 0, SEEK_CUR));
}

TEST_F(Preadv2Flags, UnknownFlagsPrecedeDestinationFaultAndPreservePosition) {
  iovec vector = {bad_, 1};
  ASSERT_EQ(2, lseek(fd_, 2, SEEK_SET));
  for (int64_t offset : {int64_t(-1), int64_t(0)}) {
    ExpectError(EOPNOTSUPP, fd_, &vector, 1, offset, kUnknown);
    EXPECT_EQ(2, lseek(fd_, 0, SEEK_CUR));
  }
}

TEST_F(Preadv2Flags, ReadIteratorAcceptsSyncAppendAndHipriFlags) {
  char bytes[2] = {};
  iovec vector = {bytes, sizeof(bytes)};
  for (unsigned flags : {kHipri, kDsync, kSync, kAppend, kHipri | kDsync | kSync | kAppend}) {
    ASSERT_EQ(1, lseek(fd_, 1, SEEK_SET));
    ASSERT_EQ(2, Preadv2(fd_, &vector, 1, 3, flags));
    EXPECT_EQ("de", std::string(bytes, sizeof(bytes)));
    EXPECT_EQ(1, lseek(fd_, 0, SEEK_CUR));
    ASSERT_EQ(2, Preadv2(fd_, &vector, 1, -1, flags));
    EXPECT_EQ("bc", std::string(bytes, sizeof(bytes)));
    EXPECT_EQ(3, lseek(fd_, 0, SEEK_CUR));
  }
}

TEST_F(Preadv2Flags, NonseekableOffsetRejectsBeforeImportAndFlags) {
  int pair[2];
  ASSERT_EQ(0, pipe2(pair, O_NONBLOCK | O_CLOEXEC));
  ExpectError(ESPIPE, pair[0], static_cast<iovec*>(bad_), 1, 0, kUnknown);
  ExpectError(ESPIPE, pair[1], nullptr, 0, 0, kUnknown);
  EXPECT_EQ(0, Preadv2(pair[0], nullptr, 0, -1, kUnknown));
  close(pair[0]);
  close(pair[1]);
}

TEST_F(Preadv2Flags, LegacyReaderAcceptsHipriButRejectsOtherFlags) {
  const int notify = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
  ASSERT_GE(notify, 0) << strerror(errno);
  char bytes[4096];
  iovec vector = {bytes, sizeof(bytes)};
  ExpectError(EAGAIN, notify, &vector, 1, -1, kHipri);
  for (unsigned flags : {kDsync, kSync, kAppend, kNowait, kUnknown}) {
    ExpectError(EOPNOTSUPP, notify, &vector, 1, -1, flags);
    EXPECT_EQ(0, Preadv2(notify, nullptr, 0, -1, flags));
  }
  close(notify);
}

TEST_F(Preadv2Flags, OffsetRangeValidationPrecedesFlags) {
  char byte;
  iovec vector = {&byte, 1};
  ExpectError(EINVAL, fd_, &vector, 1, INT64_MAX, kUnknown);
}

TEST_F(Preadv2Flags, PathOnlyDescriptorIsRejectedBeforeImport) {
  const int path_only = open(path_.c_str(), O_PATH | O_CLOEXEC);
  ASSERT_GE(path_only, 0);
  for (int64_t offset : {int64_t(-1), int64_t(0)}) {
    ExpectError(EBADF, path_only, static_cast<iovec*>(bad_), 1, offset, kUnknown);
  }
  close(path_only);
}

TEST_F(Preadv2Flags, NowaitCannotSilentlyBlockOrChangeDescriptorFlags) {
  int pair[2];
  ASSERT_EQ(0, pipe2(pair, O_CLOEXEC));
  const pid_t child = fork();
  ASSERT_GE(child, 0);
  if (child == 0) {
    alarm(2);
    char byte;
    iovec vector = {&byte, 1};
    errno = 0;
    const ssize_t result = Preadv2(pair[0], &vector, 1, -1, kNowait);
    const int error = errno;
    const int flags = fcntl(pair[0], F_GETFL);
    // A backend may explicitly decline NOWAIT. Accepting it must provide
    // per-operation nonblocking semantics without changing the shared file.
    _exit(result == -1 && (error == EAGAIN || error == EOPNOTSUPP) &&
                  flags >= 0 && !(flags & O_NONBLOCK) ? 0 : 1);
  }
  int status = 0;
  pid_t result;
  do {
    result = waitpid(child, &status, 0);
  } while (result < 0 && errno == EINTR);
  close(pair[0]);
  close(pair[1]);
  ASSERT_EQ(child, result);
  ASSERT_TRUE(WIFEXITED(status)) << "NOWAIT blocked until alarm, status=" << status;
  EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST_F(Preadv2Flags, AccessEventsOccurOnlyAtSuccessfulLogicalReadBoundary) {
  const int notify = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
  ASSERT_GE(notify, 0);
  const int watch = inotify_add_watch(notify, path_.c_str(), IN_ACCESS);
  if (watch < 0) {
    close(notify);
    FAIL() << strerror(errno);
    return;
  }
  auto expect_events = [&](bool access) {
    alignas(inotify_event) char events[1024];
    errno = 0;
    const ssize_t size = read(notify, events, sizeof(events));
    if (!access) {
      EXPECT_EQ(-1, size);
      EXPECT_EQ(EAGAIN, errno);
      return;
    }
    ASSERT_EQ(static_cast<ssize_t>(sizeof(inotify_event)), size);
    const auto* event = reinterpret_cast<const inotify_event*>(events);
    EXPECT_EQ(watch, event->wd);
    EXPECT_EQ(static_cast<unsigned>(IN_ACCESS), event->mask);
    EXPECT_EQ(0U, event->len);
  };
  char byte;
  iovec vector = {&byte, 1};
  for (int64_t offset : {int64_t(-1), int64_t(0)}) {
    ExpectError(EOPNOTSUPP, fd_, &vector, 1, offset, kUnknown);
    expect_events(false);
    ExpectError(EFAULT, fd_, static_cast<iovec*>(bad_), 1, offset, 0);
    expect_events(false);
    EXPECT_EQ(0, Preadv2(fd_, nullptr, 0, offset, kUnknown));
    expect_events(true);
    EXPECT_EQ(1, Preadv2(fd_, &vector, 1, offset, kHipri));
    expect_events(true);
  }
  close(notify);
}

TEST_F(Preadv2Flags, PidProcLegacyReadersRejectIteratorOnlyReadHints) {
  // Linux 6.6 PID ONE()/REG() entries use .read (seq_read or a custom
  // legacy reader), unlike global proc_create_single() and sysctl entries.
  for (const char* path : {"/proc/self/stat", "/proc/self/statm", "/proc/self/status",
                           "/proc/self/cmdline", "/proc/self/maps", "/proc/self/limits"}) {
    SCOPED_TRACE(path);
    const int fd = open(path, O_RDONLY | O_CLOEXEC);
    ASSERT_GE(fd, 0) << strerror(errno);
    char byte;
    iovec vector = {&byte, 1};
    for (int64_t offset : {int64_t(-1), int64_t(0)}) {
      for (unsigned flags : {kDsync, kSync, kAppend}) {
        EXPECT_EQ(0, lseek(fd, 0, SEEK_SET));
        ExpectError(EOPNOTSUPP, fd, &vector, 1, offset, flags);
        EXPECT_EQ(0, lseek(fd, 0, SEEK_CUR));
        // Empty logical reads finish before either legacy or iterator flag
        // validation, and therefore remain successful on the same object.
        EXPECT_EQ(0, Preadv2(fd, nullptr, 0, offset, flags | kUnknown));
        EXPECT_EQ(0, lseek(fd, 0, SEEK_CUR));
      }
      EXPECT_EQ(0, lseek(fd, 0, SEEK_SET));
      EXPECT_EQ(1, Preadv2(fd, &vector, 1, offset, kHipri));
      EXPECT_EQ(offset == -1 ? 1 : 0, lseek(fd, 0, SEEK_CUR));
    }
    close(fd);
  }
}

TEST_F(Preadv2Flags, BufferedIteratorProvidersAcceptReadHints) {
  // Rejecting all buffered reads or all procfs files would break these
  // Linux read_iter providers. No pathname-based special case is required.
  for (const char* path : {"/proc/meminfo", "/proc/sys/kernel/pid_max", "/dev/null"}) {
    SCOPED_TRACE(path);
    const int fd = open(path, O_RDONLY | O_CLOEXEC);
    ASSERT_GE(fd, 0) << strerror(errno);
    const bool null_device = strcmp(path, "/dev/null") == 0;
    char byte;
    iovec vector = {&byte, 1};
    for (int64_t offset : {int64_t(-1), int64_t(0)}) {
      for (unsigned flags : {kHipri, kDsync, kSync, kAppend}) {
        // DragonOS's pre-existing character-device llseek policy is separate
        // from PREAD capability. Null returns EOF without needing a seek.
        if (!null_device) {
          EXPECT_EQ(0, lseek(fd, 0, SEEK_SET));
        }
        EXPECT_EQ(null_device ? 0 : 1, Preadv2(fd, &vector, 1, offset, flags));
        if (!null_device) {
          EXPECT_EQ(offset == -1 ? 1 : 0, lseek(fd, 0, SEEK_CUR));
        }
        EXPECT_EQ(0, Preadv2(fd, nullptr, 0, offset, kUnknown));
      }
    }
    close(fd);
  }
}

TEST_F(Preadv2Flags, DirectoryLegacyReadFlagsPrecedeEisdir) {
  // Linux directories expose legacy generic_read_dir, so iterator-only
  // hints are rejected before the backend reports EISDIR.
  for (const char* path : {"/tmp", "/proc", "/proc/self"}) {
    SCOPED_TRACE(path);
    const int fd = open(path, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    ASSERT_GE(fd, 0) << strerror(errno);
    char byte;
    iovec vector = {&byte, 1};
    for (int64_t offset : {int64_t(-1), int64_t(0)}) {
      for (unsigned flags : {kDsync, kSync, kAppend, kUnknown}) {
        ExpectError(EOPNOTSUPP, fd, &vector, 1, offset, flags);
        EXPECT_EQ(0, Preadv2(fd, nullptr, 0, offset, flags));
      }
      ExpectError(EISDIR, fd, &vector, 1, offset, kHipri);
      ExpectError(EISDIR, fd, &vector, 1, offset, 0);
    }
    close(fd);
  }
}

TEST_F(Preadv2Flags, NamespaceDescriptorLacksReadCapabilityEvenForZeroLength) {
  // nsfs has neither .read nor .read_iter. FMODE_READ alone must not make
  // it readable: CAN_READ fails before zero-count completion and RWF checks.
  const int fd = open("/proc/self/ns/mnt", O_RDONLY | O_CLOEXEC);
  ASSERT_GE(fd, 0) << strerror(errno);
  char byte;
  iovec vector = {&byte, 1};
  for (int64_t offset : {int64_t(-1), int64_t(0)}) {
    for (unsigned flags : {0U, kHipri, kDsync, kSync, kAppend, kNowait, kUnknown}) {
      ExpectError(EINVAL, fd, &vector, 1, offset, flags);
      ExpectError(EINVAL, fd, nullptr, 0, offset, flags);
    }
  }
  close(fd);
}

TEST_F(Preadv2Flags, NativeOffsetUsesFullLowWordAndIgnoresHighWord) {
  char byte = 0;
  iovec vector = {&byte, 1};
  ASSERT_EQ(1, Preadv2(fd_, &vector, 1, 1, 0, 1));
  EXPECT_EQ('b', byte);
  // A high offset cannot be silently narrowed to the first few file bytes.
  EXPECT_EQ(0, Preadv2(fd_, &vector, 1, (int64_t(1) << 32) + 1, 0));
  ASSERT_EQ(2, lseek(fd_, 2, SEEK_SET));
  ASSERT_EQ(1, Preadv2(fd_, &vector, 1, -1, 0, 0x12345678));
  EXPECT_EQ('c', byte);
  EXPECT_EQ(3, lseek(fd_, 0, SEEK_CUR));
}

TEST_F(Preadv2Flags, NativeFlagsAreTruncatedToRwfTWidth) {
  char byte = 0;
  iovec vector = {&byte, 1};
  ASSERT_EQ(1, Preadv2(fd_, &vector, 1, 0, 1UL << 32));
  EXPECT_EQ('a', byte);
}

}  // namespace

int main(int argc, char** argv) {
  testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
