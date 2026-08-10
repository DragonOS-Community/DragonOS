#include <fcntl.h>
#include <gtest/gtest.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/statvfs.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <atomic>
#include <cerrno>
#include <cstring>
#include <string>
#include <thread>
#include <vector>

namespace {

constexpr size_t kWorkers = 8;
constexpr size_t kBlockSize = 4096;
constexpr size_t kBlocksPerFile = 64;
constexpr long kMsdosSuperMagic = 0x4d44;

std::string TestPath(size_t worker) {
  return "/fat-concurrent-allocation-" + std::to_string(getpid()) + "-" +
         std::to_string(worker);
}

bool WriteAll(int fd, const uint8_t* data, size_t len) {
  while (len != 0) {
    const ssize_t written = write(fd, data, len);
    if (written < 0) {
      if (errno == EINTR) {
        continue;
      }
      return false;
    }
    if (written == 0) {
      return false;
    }
    data += written;
    len -= static_cast<size_t>(written);
  }
  return true;
}

bool ReadAllAt(int fd, uint8_t* data, size_t len, off_t offset) {
  while (len != 0) {
    const ssize_t bytes_read = pread(fd, data, len, offset);
    if (bytes_read < 0) {
      if (errno == EINTR) {
        continue;
      }
      return false;
    }
    if (bytes_read == 0) {
      errno = EIO;
      return false;
    }
    data += bytes_read;
    len -= static_cast<size_t>(bytes_read);
    offset += bytes_read;
  }
  return true;
}

class TestFiles {
 public:
  TestFiles() { fds.fill(-1); }

  ~TestFiles() {
    for (size_t worker = 0; worker < kWorkers; ++worker) {
      if (fds[worker] >= 0) {
        close(fds[worker]);
      }
      unlink(TestPath(worker).c_str());
    }
  }

  std::array<int, kWorkers> fds;
};

TEST(FatConcurrentAllocation, ParallelGrowthKeepsClusterChainsIndependent) {
  struct statfs fs_type {};
  ASSERT_EQ(0, statfs("/", &fs_type)) << strerror(errno);
  if (fs_type.f_type != kMsdosSuperMagic) {
    GTEST_SKIP() << "root filesystem is not FAT";
  }

  struct statvfs space {};
  ASSERT_EQ(0, statvfs("/", &space)) << strerror(errno);
  const uint64_t required =
      kWorkers * kBlockSize * kBlocksPerFile + 1024 * 1024;
  if (space.f_bavail * space.f_frsize < required) {
    GTEST_SKIP() << "insufficient free FAT space for allocation stress";
  }

  TestFiles files;
  for (size_t worker = 0; worker < kWorkers; ++worker) {
    const std::string path = TestPath(worker);
    unlink(path.c_str());
    files.fds[worker] = open(path.c_str(), O_CREAT | O_TRUNC | O_RDWR, 0600);
    ASSERT_GE(files.fds[worker], 0) << path << ": " << strerror(errno);
  }

  std::atomic<size_t> ready{0};
  std::atomic<bool> start{false};
  std::array<int, kWorkers> worker_errno{};
  std::vector<std::thread> threads;
  threads.reserve(kWorkers);
  for (size_t worker = 0; worker < kWorkers; ++worker) {
    threads.emplace_back([&, worker] {
      const uint8_t fill = static_cast<uint8_t>(0x31 + worker);
      std::array<uint8_t, kBlockSize> block{};
      block.fill(fill);
      ready.fetch_add(1, std::memory_order_release);
      while (!start.load(std::memory_order_acquire)) {
        std::this_thread::yield();
      }
      for (size_t block_index = 0; block_index < kBlocksPerFile;
           ++block_index) {
        if (!WriteAll(files.fds[worker], block.data(), block.size())) {
          worker_errno[worker] = errno == 0 ? EIO : errno;
          return;
        }
      }
      if (fsync(files.fds[worker]) != 0) {
        worker_errno[worker] = errno;
      }
    });
  }

  while (ready.load(std::memory_order_acquire) != kWorkers) {
    std::this_thread::yield();
  }
  start.store(true, std::memory_order_release);
  for (auto& thread : threads) {
    thread.join();
  }

  std::array<uint8_t, kBlockSize> block{};
  for (size_t worker = 0; worker < kWorkers; ++worker) {
    ASSERT_EQ(0, worker_errno[worker])
        << TestPath(worker) << ": " << strerror(worker_errno[worker]);
    const uint8_t expected = static_cast<uint8_t>(0x31 + worker);
    for (size_t block_index = 0; block_index < kBlocksPerFile;
         ++block_index) {
      const off_t offset = static_cast<off_t>(block_index * kBlockSize);
      ASSERT_TRUE(
          ReadAllAt(files.fds[worker], block.data(), block.size(), offset))
          << TestPath(worker) << " block " << block_index << ": "
          << strerror(errno);
      ASSERT_TRUE(std::all_of(block.begin(), block.end(),
                              [expected](uint8_t value) {
                                return value == expected;
                              }))
          << TestPath(worker) << " block " << block_index;
    }
    ASSERT_EQ(0, close(files.fds[worker])) << strerror(errno);
    files.fds[worker] = -1;
    ASSERT_EQ(0, unlink(TestPath(worker).c_str())) << strerror(errno);
  }
}

}  // namespace

int main(int argc, char** argv) {
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
