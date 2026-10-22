#include <gtest/gtest.h>

#include <fcntl.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/statfs.h>
#include <sys/wait.h>
#include <unistd.h>

#include <cerrno>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <fstream>
#include <string>

namespace {
constexpr size_t kMiB = 1024 * 1024;

uint64_t MemTotalKiB() {
  std::ifstream input("/proc/meminfo");
  std::string key, unit;
  uint64_t value;
  while (input >> key >> value >> unit) {
    if (key == "MemTotal:" && unit == "kB") return value;
  }
  return 0;
}

// Populate clean file cache before anonymous faults consume physical memory.
// No dirty I/O, model service, or kernel fault injection is involved.
int ExerciseCleanReclaim(int fd) {
  constexpr size_t file_bytes = 192 * kMiB;
  constexpr size_t anonymous_bytes = 700 * kMiB;
  if (ftruncate(fd, file_bytes) != 0) return 10;
  void* file = mmap(nullptr, file_bytes, PROT_READ, MAP_SHARED, fd, 0);
  if (file == MAP_FAILED) return 11;
  auto* bytes = static_cast<volatile unsigned char*>(file);
  for (size_t offset = 0; offset < file_bytes; offset += 4096) {
    if (bytes[offset] != 0) return 12;
  }
  if (munmap(file, file_bytes) != 0) return 13;
  void* anonymous = mmap(nullptr, anonymous_bytes, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (anonymous == MAP_FAILED) return 14;
  bytes = static_cast<volatile unsigned char*>(anonymous);
  for (size_t offset = 0; offset < anonymous_bytes; offset += 4096)
    bytes[offset] = 37;
  for (size_t offset = 0; offset < anonymous_bytes; offset += 4096) {
    if (bytes[offset] != 37) return 15;
  }
  return munmap(anonymous, anonymous_bytes) == 0 ? 0 : 16;
}

TEST(GlobalReclaim, CleanCacheRemainsReclaimableWhenPreparationCannotAllocate) {
  // A capacity regression, run in the dedicated 1 GiB guest. Larger guests
  // would pass without entering physical reclaim and provide false coverage.
  const uint64_t total = MemTotalKiB();
  ASSERT_NE(total, 0u);
  if (total < 960 * 1024 || total > 1024 * 1024)
    GTEST_SKIP() << "requires a 1 GiB guest";
  char path[] = "/var/tmp/global-reclaim-XXXXXX";
  int fd = mkstemp(path);
  ASSERT_GE(fd, 0);
  struct statfs fs;
  int stat_result = fstatfs(fd, &fs);
  if (stat_result != 0) {
    const int error = errno;
    close(fd);
    unlink(path);
    FAIL() << "fstatfs failed: " << error;
  }
  // Keep an ordinary filesystem cache; tmpfs data cannot be discarded.
  if (fs.f_type != 0xef53) {
    close(fd);
    unlink(path);
    GTEST_SKIP() << "requires ext4 scratch storage";
  }
  pid_t child = fork();
  if (child == 0) _exit(ExerciseCleanReclaim(fd));
  if (child < 0) {
    close(fd);
    unlink(path);
    FAIL() << "fork failed: " << errno;
  }
  int status = 0;
  pid_t result = 0;
  for (int i = 0; i < 12000 && result == 0; ++i) {
    result = waitpid(child, &status, WNOHANG);
    if (result < 0 && errno == EINTR) result = 0;
    if (result == 0) usleep(10000);
  }
  if (result != child) {
    kill(child, SIGKILL);
    while (waitpid(child, &status, 0) < 0 && errno == EINTR) {}
  }
  close(fd);
  unlink(path);
  ASSERT_EQ(result, child) << "reclaim child failed to finish within 120s";
  ASSERT_TRUE(WIFEXITED(status)) << "unexpected signal: " << status;
  EXPECT_EQ(WEXITSTATUS(status), 0);
}
}  // namespace

int main(int argc, char** argv) {
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
