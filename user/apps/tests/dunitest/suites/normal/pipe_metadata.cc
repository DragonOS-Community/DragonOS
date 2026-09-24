#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <linux/capability.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include <string>

namespace {

class PipeFds {
 public:
  ~PipeFds() {
    for (int fd : fds_) {
      if (fd >= 0) close(fd);
    }
  }

  bool Open() { return pipe2(fds_, O_CLOEXEC) == 0; }
  int read_fd() const { return fds_[0]; }
  int write_fd() const { return fds_[1]; }

 private:
  int fds_[2] = {-1, -1};
};

class TempFifo {
 public:
  ~TempFifo() {
    if (!path_.empty()) unlink(path_.c_str());
    if (!dir_.empty()) rmdir(dir_.c_str());
  }

  bool Create() {
    char name[] = "/tmp/pipe-metadata-XXXXXX";
    char* dir = mkdtemp(name);
    if (dir == nullptr) return false;
    dir_ = dir;
    path_ = dir_ + "/fifo";
    return mkfifo(path_.c_str(), 0600) == 0;
  }

  const std::string& path() const { return path_; }

 private:
  std::string dir_;
  std::string path_;
};

void ExpectSameIdentityAndAttributes(const struct stat& by_fd, const struct stat& by_path) {
  EXPECT_EQ(by_fd.st_dev, by_path.st_dev);
  EXPECT_EQ(by_fd.st_ino, by_path.st_ino);
  EXPECT_EQ(by_fd.st_uid, by_path.st_uid);
  EXPECT_EQ(by_fd.st_gid, by_path.st_gid);
  EXPECT_EQ(by_fd.st_mode, by_path.st_mode);
  EXPECT_EQ(by_fd.st_atim.tv_sec, by_path.st_atim.tv_sec);
  EXPECT_EQ(by_fd.st_mtim.tv_sec, by_path.st_mtim.tv_sec);
}

bool DropEffectiveCapability(unsigned int cap) {
  struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
  struct __user_cap_data_struct data[2] = {};
  if (syscall(SYS_capget, &header, data) != 0) return false;
  data[cap / 32].effective &= ~(1U << (cap % 32));
  return syscall(SYS_capset, &header, data) == 0;
}

bool WriteUidMap(unsigned int parent_uid) {
  int fd = open("/proc/self/uid_map", O_WRONLY);
  if (fd < 0) return false;
  char map[64] = {};
  const int size = snprintf(map, sizeof(map), "0 %u 1\n", parent_uid);
  const bool written = size > 0 && size < static_cast<int>(sizeof(map)) &&
                       write(fd, map, size) == size;
  close(fd);
  return written;
}

}  // namespace

TEST(PipeMetadata, AnonymousPipeInitialAttributesAndSharedChown) {
  PipeFds pipe;
  ASSERT_TRUE(pipe.Open()) << strerror(errno);
  struct stat read_stat = {};
  struct stat write_stat = {};
  ASSERT_EQ(0, fstat(pipe.read_fd(), &read_stat)) << strerror(errno);
  ASSERT_EQ(0, fstat(pipe.write_fd(), &write_stat)) << strerror(errno);
  EXPECT_TRUE(S_ISFIFO(read_stat.st_mode));
  EXPECT_EQ(static_cast<mode_t>(0600), read_stat.st_mode & 07777);
  EXPECT_EQ(0, read_stat.st_size);
  EXPECT_EQ(geteuid(), read_stat.st_uid);
  EXPECT_EQ(getegid(), read_stat.st_gid);
  EXPECT_GT(read_stat.st_ctim.tv_sec, 0);
  EXPECT_EQ(read_stat.st_ino, write_stat.st_ino);

  if (geteuid() != 0) GTEST_SKIP() << "ownership change needs root";
  ASSERT_EQ(0, fchown(pipe.read_fd(), 1234, 5678)) << strerror(errno);
  ASSERT_EQ(0, fstat(pipe.write_fd(), &write_stat)) << strerror(errno);
  EXPECT_EQ(1234U, write_stat.st_uid);
  EXPECT_EQ(5678U, write_stat.st_gid);
  ASSERT_EQ(0, fchown(pipe.write_fd(), -1, -1)) << strerror(errno);
  ASSERT_EQ(0, fstat(pipe.read_fd(), &read_stat)) << strerror(errno);
  EXPECT_EQ(1234U, read_stat.st_uid);
  EXPECT_EQ(5678U, read_stat.st_gid);
  ASSERT_EQ(0, fchmod(pipe.read_fd(), 0640)) << strerror(errno);
  ASSERT_EQ(0, fstat(pipe.write_fd(), &write_stat)) << strerror(errno);
  EXPECT_TRUE(S_ISFIFO(write_stat.st_mode));
  EXPECT_EQ(static_cast<mode_t>(0640), write_stat.st_mode & 07777);
  EXPECT_EQ(-1, fchown(-1, 0, 0));
  EXPECT_EQ(EBADF, errno);
}

TEST(PipeMetadata, CapacityDoesNotBecomeFileSize) {
  PipeFds pipe;
  ASSERT_TRUE(pipe.Open()) << strerror(errno);
  const int capacity = fcntl(pipe.read_fd(), F_GETPIPE_SZ);
  ASSERT_GT(capacity, 0) << strerror(errno);
  struct stat before = {};
  ASSERT_EQ(0, fstat(pipe.read_fd(), &before)) << strerror(errno);
  EXPECT_EQ(0, before.st_size);

  const int resized = fcntl(pipe.read_fd(), F_SETPIPE_SZ, 4096);
  ASSERT_GT(resized, 0) << strerror(errno);
  EXPECT_EQ(resized, fcntl(pipe.write_fd(), F_GETPIPE_SZ));
  struct stat after = {};
  ASSERT_EQ(0, fstat(pipe.write_fd(), &after)) << strerror(errno);
  EXPECT_EQ(0, after.st_size);
}

TEST(PipeMetadata, ProcFdUtimesUpdatesAnonymousPipe) {
  PipeFds pipe;
  ASSERT_TRUE(pipe.Open()) << strerror(errno);
  char proc_path[64] = {};
  snprintf(proc_path, sizeof(proc_path), "/proc/self/fd/%d", pipe.read_fd());
  struct timeval times[2] = {{1700000000, 0}, {1700000001, 0}};
  ASSERT_EQ(0, utimes(proc_path, times)) << strerror(errno);
  struct stat st = {};
  ASSERT_EQ(0, fstat(pipe.write_fd(), &st)) << strerror(errno);
  EXPECT_EQ(times[0].tv_sec, st.st_atim.tv_sec);
  EXPECT_EQ(times[1].tv_sec, st.st_mtim.tv_sec);
}

TEST(PipeMetadata, NamedFifoFdAttributesUsePersistentInode) {
  if (geteuid() != 0) GTEST_SKIP() << "ownership change needs root";
  TempFifo fifo;
  ASSERT_TRUE(fifo.Create()) << strerror(errno);
  int fd = open(fifo.path().c_str(), O_RDWR | O_NONBLOCK);
  ASSERT_GE(fd, 0) << strerror(errno);

  struct stat by_fd = {};
  struct stat by_path = {};
  ASSERT_EQ(0, fstat(fd, &by_fd)) << strerror(errno);
  ASSERT_EQ(0, stat(fifo.path().c_str(), &by_path)) << strerror(errno);
  ExpectSameIdentityAndAttributes(by_fd, by_path);

  ASSERT_EQ(0, fchown(fd, 1234, 5678)) << strerror(errno);
  ASSERT_EQ(0, fchmod(fd, 0640)) << strerror(errno);
  struct timespec times[2] = {{1700000000, 0}, {1700000001, 0}};
  ASSERT_EQ(0, futimens(fd, times)) << strerror(errno);
  ASSERT_EQ(0, fstat(fd, &by_fd)) << strerror(errno);
  ASSERT_EQ(0, stat(fifo.path().c_str(), &by_path)) << strerror(errno);
  ExpectSameIdentityAndAttributes(by_fd, by_path);
  EXPECT_TRUE(S_ISFIFO(by_fd.st_mode));
  EXPECT_EQ(1234U, by_fd.st_uid);
  EXPECT_EQ(5678U, by_fd.st_gid);
  EXPECT_EQ(static_cast<mode_t>(0640), by_fd.st_mode & 07777);
  EXPECT_EQ(times[0].tv_sec, by_fd.st_atim.tv_sec);
  EXPECT_EQ(times[1].tv_sec, by_fd.st_mtim.tv_sec);

  ASSERT_EQ(0, close(fd)) << strerror(errno);
  fd = open(fifo.path().c_str(), O_RDWR | O_NONBLOCK);
  ASSERT_GE(fd, 0) << strerror(errno);
  ASSERT_EQ(0, fstat(fd, &by_fd)) << strerror(errno);
  ASSERT_EQ(0, stat(fifo.path().c_str(), &by_path)) << strerror(errno);
  ExpectSameIdentityAndAttributes(by_fd, by_path);
  EXPECT_EQ(1234U, by_fd.st_uid);
  EXPECT_EQ(5678U, by_fd.st_gid);
  EXPECT_EQ(static_cast<mode_t>(0640), by_fd.st_mode & 07777);
  close(fd);
}

TEST(PipeMetadata, UnprivilegedProcessCannotTakeAnotherOwnersPipe) {
  if (geteuid() != 0) GTEST_SKIP() << "requires root to drop privileges";
  PipeFds pipe;
  ASSERT_TRUE(pipe.Open()) << strerror(errno);
  const pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    if (setgid(65534) != 0 || setuid(65534) != 0) _exit(2);
    if (fchown(pipe.read_fd(), -1, -1) != 0) _exit(4);
    errno = 0;
    const int result = fchown(pipe.read_fd(), 65534, 65534);
    _exit(result == -1 && errno == EPERM ? 0 : 3);
  }
  int status = 0;
  ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(PipeMetadata, FsuidNotRealUidControlsPipeOwnership) {
  if (geteuid() != 0) GTEST_SKIP() << "requires root to change credentials";
  const pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    if (setresuid(1234, 0, 0) != 0) _exit(2);
    int fds[2] = {-1, -1};
    if (pipe2(fds, O_CLOEXEC) != 0) _exit(3);
    struct stat st = {};
    if (fstat(fds[0], &st) != 0 || st.st_uid != 0) _exit(4);
    if (fchown(fds[0], -1, -1) != 0) _exit(5);
    _exit(0);
  }
  int status = 0;
  ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(PipeMetadata, NonOwnerCannotClearSetuidViaNoopChown) {
  if (geteuid() != 0) GTEST_SKIP() << "requires root to drop privileges";
  PipeFds pipe;
  ASSERT_TRUE(pipe.Open()) << strerror(errno);
  ASSERT_EQ(0, fchmod(pipe.read_fd(), 04600)) << strerror(errno);
  const pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    if (setgid(65534) != 0 || setuid(65534) != 0) _exit(2);
    errno = 0;
    const int result = fchown(pipe.read_fd(), -1, -1);
    _exit(result == -1 && errno == EPERM ? 0 : 3);
  }
  int status = 0;
  ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(PipeMetadata, ChmodClearsSetgidOutsideOwningGroup) {
  if (geteuid() != 0) GTEST_SKIP() << "requires root to drop privileges";
  PipeFds pipe;
  ASSERT_TRUE(pipe.Open()) << strerror(errno);
  ASSERT_EQ(0, fchown(pipe.read_fd(), 1234, 5678)) << strerror(errno);
  const pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    if (setgid(1234) != 0 || setuid(1234) != 0) _exit(2);
    if (fchmod(pipe.read_fd(), 02770) != 0) _exit(3);
    struct stat st = {};
    if (fstat(pipe.write_fd(), &st) != 0) _exit(4);
    _exit((st.st_mode & S_ISGID) == 0 ? 0 : 5);
  }
  int status = 0;
  ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(PipeMetadata, RootWithoutCapChownCannotTakePipeOwnership) {
  if (geteuid() != 0) GTEST_SKIP() << "requires root to drop capability";
  PipeFds pipe;
  ASSERT_TRUE(pipe.Open()) << strerror(errno);
  const pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    if (!DropEffectiveCapability(CAP_CHOWN)) _exit(2);
    errno = 0;
    const int result = fchown(pipe.read_fd(), 1234, 5678);
    _exit(result == -1 && errno == EPERM ? 0 : 3);
  }
  int status = 0;
  ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(PipeMetadata, ChownModeUpdateChecksRequestedGroupForSetgid) {
  if (geteuid() != 0) GTEST_SKIP() << "requires root to drop capability";
  PipeFds pipe;
  ASSERT_TRUE(pipe.Open()) << strerror(errno);
  ASSERT_EQ(0, fchmod(pipe.read_fd(), 06660)) << strerror(errno);
  const pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    if (!DropEffectiveCapability(CAP_FSETID)) _exit(2);
    if (fchown(pipe.read_fd(), -1, 5678) != 0) _exit(3);
    struct stat st = {};
    if (fstat(pipe.write_fd(), &st) != 0) _exit(4);
    _exit((st.st_mode & (S_ISUID | S_ISGID)) == 0 ? 0 : 5);
  }
  int status = 0;
  ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(PipeMetadata, NonOwnerCannotSetExplicitTimes) {
  if (geteuid() != 0) GTEST_SKIP() << "requires root to drop privileges";
  PipeFds pipe;
  ASSERT_TRUE(pipe.Open()) << strerror(errno);
  const pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    if (setgid(65534) != 0 || setuid(65534) != 0) _exit(2);
    struct timespec omitted[2] = {{0, UTIME_OMIT}, {0, UTIME_OMIT}};
    if (futimens(pipe.read_fd(), omitted) != 0) _exit(4);
    struct timespec times[2] = {{1700000000, 0}, {1700000001, 0}};
    errno = 0;
    const int result = futimens(pipe.read_fd(), times);
    _exit(result == -1 && errno == EPERM ? 0 : 3);
  }
  int status = 0;
  ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(PipeMetadata, OwnershipTransferRevokesFdAttributeWrites) {
  if (geteuid() != 0) GTEST_SKIP() << "requires root to transfer ownership";
  PipeFds target;
  ASSERT_TRUE(target.Open()) << strerror(errno);
  ASSERT_EQ(0, fchown(target.read_fd(), 1234, 1234)) << strerror(errno);
  int ready[2] = {-1, -1};
  int proceed[2] = {-1, -1};
  ASSERT_EQ(0, pipe(ready)) << strerror(errno);
  ASSERT_EQ(0, pipe(proceed)) << strerror(errno);
  const pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    close(ready[0]);
    close(proceed[1]);
    if (setgid(1234) != 0 || setuid(1234) != 0) _exit(2);
    char marker = 'x';
    if (write(ready[1], &marker, 1) != 1) _exit(3);
    if (read(proceed[0], &marker, 1) != 1) _exit(4);
    errno = 0;
    if (fchmod(target.read_fd(), 0644) != -1 || errno != EPERM) _exit(5);
    struct timespec times[2] = {{1700000000, 0}, {1700000001, 0}};
    errno = 0;
    if (futimens(target.read_fd(), times) != -1 || errno != EPERM) _exit(6);
    char proc_path[64] = {};
    snprintf(proc_path, sizeof(proc_path), "/proc/self/fd/%d", target.read_fd());
    struct timeval tv[2] = {{1700000000, 0}, {1700000001, 0}};
    errno = 0;
    const int result = utimes(proc_path, tv);
    // Path lookup may reject the procfd earlier with EACCES; either error
    // must leave the inherited pipe's attributes unchanged.
    if (result != -1 || (errno != EPERM && errno != EACCES)) {
      fprintf(stderr, "utimes(procfd) result=%d errno=%d\n", result, errno);
      _exit(7);
    }
    _exit(0);
  }
  close(ready[1]);
  close(proceed[0]);
  char marker = 0;
  ASSERT_EQ(1, read(ready[0], &marker, 1)) << strerror(errno);
  ASSERT_EQ(0, fchown(target.write_fd(), 5678, 5678)) << strerror(errno);
  ASSERT_EQ(1, write(proceed[1], &marker, 1)) << strerror(errno);
  close(ready[0]);
  close(proceed[1]);
  int status = 0;
  ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
  struct stat st = {};
  ASSERT_EQ(0, fstat(target.read_fd(), &st)) << strerror(errno);
  EXPECT_EQ(5678U, st.st_uid);
  EXPECT_EQ(static_cast<mode_t>(0600), st.st_mode & 07777);
}

TEST(PipeMetadata, UnmappedInodeCannotBeChangedWithUsernsCapabilities) {
  if (geteuid() != 0) GTEST_SKIP() << "requires root and user namespaces";
  PipeFds pipe;
  ASSERT_TRUE(pipe.Open()) << strerror(errno);
  ASSERT_EQ(0, fchown(pipe.read_fd(), 1234, 1234)) << strerror(errno);
  TempFifo fifo;
  ASSERT_TRUE(fifo.Create()) << strerror(errno);
  ASSERT_EQ(0, chown(fifo.path().c_str(), 1234, 1234)) << strerror(errno);
  int fifo_fd = open(fifo.path().c_str(), O_RDWR | O_NONBLOCK);
  ASSERT_GE(fifo_fd, 0) << strerror(errno);

  const pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    if (unshare(CLONE_NEWUSER) != 0) _exit(77);
    if (!WriteUidMap(0)) _exit(77);
    struct timespec times[2] = {{1700000000, 0}, {1700000001, 0}};
    for (int fd : {pipe.read_fd(), fifo_fd}) {
      errno = 0;
      if (fchown(fd, 0, -1) != -1 || errno != EPERM) _exit(2);
      errno = 0;
      if (fchmod(fd, 0644) != -1 || errno != EPERM) _exit(3);
      errno = 0;
      if (futimens(fd, times) != -1 || errno != EPERM) _exit(4);
    }
    _exit(0);
  }
  int status = 0;
  ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
  ASSERT_TRUE(WIFEXITED(status));
  if (WEXITSTATUS(status) == 77) {
    close(fifo_fd);
    GTEST_SKIP() << "cannot create user namespace or write uid_map";
  }
  EXPECT_EQ(0, WEXITSTATUS(status));
  close(fifo_fd);
}

TEST(PipeMetadata, UserNamespaceChownTargetUsesMappedGlobalId) {
  if (geteuid() != 0) GTEST_SKIP() << "requires root and user namespaces";
  PipeFds pipe;
  ASSERT_TRUE(pipe.Open()) << strerror(errno);
  ASSERT_EQ(0, fchown(pipe.read_fd(), 1000, 1000)) << strerror(errno);
  TempFifo fifo;
  ASSERT_TRUE(fifo.Create()) << strerror(errno);
  ASSERT_EQ(0, chown(fifo.path().c_str(), 1000, 1000)) << strerror(errno);
  int fifo_fd = open(fifo.path().c_str(), O_RDWR | O_NONBLOCK);
  ASSERT_GE(fifo_fd, 0) << strerror(errno);

  const pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    if (setgid(1000) != 0 || setuid(1000) != 0) _exit(2);
    if (unshare(CLONE_NEWUSER) != 0) _exit(77);
    if (!WriteUidMap(1000)) _exit(77);
    for (int fd : {pipe.read_fd(), fifo_fd}) {
      // Namespace UID 0 denotes global UID 1000, not global UID 0.
      if (fchown(fd, 0, -1) != 0) _exit(3);
      errno = 0;
      if (fchown(fd, 1, -1) != -1 || errno != EINVAL) _exit(4);
    }
    _exit(0);
  }
  int status = 0;
  ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
  ASSERT_TRUE(WIFEXITED(status));
  if (WEXITSTATUS(status) == 77) {
    close(fifo_fd);
    GTEST_SKIP() << "cannot create user namespace or write uid_map";
  }
  EXPECT_EQ(0, WEXITSTATUS(status));
  struct stat pipe_stat = {};
  struct stat fifo_stat = {};
  ASSERT_EQ(0, fstat(pipe.read_fd(), &pipe_stat)) << strerror(errno);
  ASSERT_EQ(0, stat(fifo.path().c_str(), &fifo_stat)) << strerror(errno);
  EXPECT_EQ(1000U, pipe_stat.st_uid);
  EXPECT_EQ(1000U, fifo_stat.st_uid);
  close(fifo_fd);
}

int main(int argc, char** argv) {
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
