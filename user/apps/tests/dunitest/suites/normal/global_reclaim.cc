#include <gtest/gtest.h>

#include <fcntl.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/statfs.h>
#include <sys/stat.h>
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

// Keep this capacity workload unchanged between baseline and fixed kernels.
// Dirty mode additionally exercises writeback progress before physical retry.
int ExerciseReclaim(int fd, bool dirty) {
  constexpr size_t file_bytes = 192 * kMiB;
  constexpr size_t anonymous_bytes = 700 * kMiB;
  if (ftruncate(fd, file_bytes) != 0) return 10;
  void* file = mmap(nullptr, file_bytes, PROT_READ | (dirty ? PROT_WRITE : 0),
                    MAP_SHARED, fd, 0);
  if (file == MAP_FAILED) return 11;
  auto* bytes = static_cast<volatile unsigned char*>(file);
  for (size_t offset = 0; offset < file_bytes; offset += 4096) {
    if (bytes[offset] != 0) return 12;
    if (dirty) bytes[offset] = 53;
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
  if (munmap(anonymous, anonymous_bytes) != 0) return 16;
  if (dirty) {
    if (fsync(fd) != 0) return 17;
    unsigned char value = 0;
    for (size_t offset = 0; offset < file_bytes; offset += 4096) {
      if (pread(fd, &value, 1, offset) != 1 || value != 53) return 18;
    }
  }
  return 0;
}

uint64_t OomKills() {
  std::ifstream input("/proc/vmstat");
  std::string key;
  uint64_t value;
  while (input >> key >> value) {
    if (key == "oom_kill") return value;
  }
  ADD_FAILURE() << "oom_kill counter missing from /proc/vmstat";
  return UINT64_MAX;
}

bool WriteText(const std::string& path, const std::string& value) {
  int fd = open(path.c_str(), O_WRONLY);
  if (fd < 0) return false;
  const bool written = write(fd, value.data(), value.size()) ==
                       static_cast<ssize_t>(value.size());
  const bool closed = close(fd) == 0;
  return written && closed;
}

void CheckReclaim(bool dirty, bool protect_low = false) {
  // A capacity regression, run in the dedicated 1 GiB guest. Larger guests
  // would pass without entering physical reclaim and provide false coverage.
  const uint64_t total = MemTotalKiB();
  ASSERT_NE(total, 0u);
  if (total < 960 * 1024 || total > 1024 * 1024)
    GTEST_SKIP() << "requires a 1 GiB guest";
  const char* directory = getenv("DUNITEST_MEMORY_FILE_DIR");
  if (directory == nullptr || directory[0] == '\0') directory = "/var/tmp";
  std::string path = std::string(directory) + "/global-reclaim-XXXXXX";
  int fd = mkstemp(path.data());
  ASSERT_GE(fd, 0);
  struct statfs fs;
  int stat_result = fstatfs(fd, &fs);
  if (stat_result != 0) {
    const int error = errno;
    close(fd);
    unlink(path.c_str());
    FAIL() << "fstatfs failed: " << error;
  }
  // Keep an ordinary filesystem cache; tmpfs data cannot be discarded.
  if (fs.f_type != 0xef53) {
    close(fd);
    unlink(path.c_str());
    GTEST_SKIP() << "requires ext4 scratch storage";
  }
  std::string group;
  bool enabled_memory = false;
  if (protect_low) {
    std::ifstream controllers("/sys/fs/cgroup/cgroup.subtree_control");
    std::string controller;
    bool has_memory = false;
    while (controllers >> controller) has_memory |= controller == "memory";
    enabled_memory = !has_memory;
    group = "/sys/fs/cgroup/global-reclaim-" + std::to_string(getpid());
    const bool prepared = (!enabled_memory ||
                           WriteText("/sys/fs/cgroup/cgroup.subtree_control", "+memory")) &&
                          mkdir(group.c_str(), 0755) == 0 &&
                          WriteText(group + "/memory.low", "1G");
    if (!prepared) {
      rmdir(group.c_str());
      if (enabled_memory) WriteText("/sys/fs/cgroup/cgroup.subtree_control", "-memory");
      close(fd);
      unlink(path.c_str());
      FAIL() << "cannot prepare memory.low cgroup: " << errno;
    }
  }
  const uint64_t before = OomKills();
  pid_t child = fork();
  if (child == 0) {
    if (protect_low && !WriteText(group + "/cgroup.procs", std::to_string(getpid())))
      _exit(19);
    _exit(ExerciseReclaim(fd, dirty));
  }
  if (child < 0) {
    close(fd);
    unlink(path.c_str());
    if (protect_low) rmdir(group.c_str());
    if (enabled_memory) WriteText("/sys/fs/cgroup/cgroup.subtree_control", "-memory");
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
  unlink(path.c_str());
  if (protect_low) {
    std::ifstream events(group + "/memory.events");
    std::string event;
    uint64_t count = 0, low = 0;
    while (events >> event >> count) {
      if (event == "low") low = count;
    }
    EXPECT_GT(low, 0u) << "must exercise low-protection fallback";
    events.close();
    EXPECT_EQ(rmdir(group.c_str()), 0) << errno;
    if (enabled_memory)
      EXPECT_TRUE(WriteText("/sys/fs/cgroup/cgroup.subtree_control", "-memory"));
  }
  ASSERT_EQ(result, child) << "reclaim child failed to finish within 120s";
  ASSERT_TRUE(WIFEXITED(status)) << "unexpected signal: " << status;
  EXPECT_EQ(WEXITSTATUS(status), 0);
  EXPECT_EQ(OomKills(), before) << "reclaimable cache must not cause an OOM kill";
}

TEST(GlobalReclaim, CleanCacheRemainsReclaimableWhenPreparationCannotAllocate) {
  CheckReclaim(false);
}

TEST(GlobalReclaim, DirtyCacheMakesProgressBeforeGlobalOom) {
  CheckReclaim(true);
}
TEST(GlobalReclaim, GlobalPressureCanBreachLowAcrossBoundedScanBatches) {
  // 192 MiB is 49,152 pages, well beyond one 4,096-candidate scan.
  // memory.low must yield under global pressure; memory.min remains hard.
  CheckReclaim(false, true);
}
}  // namespace

int main(int argc, char** argv) {
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
