#include <fcntl.h>
#include <gtest/gtest.h>
#include <sys/stat.h>
#include <sys/mman.h>
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

TEST(FatConcurrentAllocation, FallocateAcrossExactClusterBoundaries) {
  struct statfs fs_type {};
  ASSERT_EQ(0, statfs("/", &fs_type)) << strerror(errno);
  if (fs_type.f_type != kMsdosSuperMagic) {
    GTEST_SKIP() << "root filesystem is not FAT";
  }

  const std::string path = TestPath(999);
  unlink(path.c_str());
  const int fd = open(path.c_str(), O_CREAT | O_TRUNC | O_RDWR, 0600);
  ASSERT_GE(fd, 0) << path << ": " << strerror(errno);

  // FAT cluster sizes are powers of two. Exercising each possible boundary
  // catches the exclusive-EOF bug without depending on a non-portable ioctl
  // for the mounted volume's cluster size.
  for (off_t size = 512; size <= 64 * 1024; size *= 2) {
    ASSERT_EQ(0, ftruncate(fd, 0)) << "size=" << size << ": " << strerror(errno);
    ASSERT_EQ(0, fallocate(fd, 0, 0, size))
        << "size=" << size << ": " << strerror(errno);

    struct stat st {};
    ASSERT_EQ(0, fstat(fd, &st)) << strerror(errno);
    ASSERT_EQ(size, st.st_size);

    uint8_t last = 0xff;
    ASSERT_EQ(1, pread(fd, &last, sizeof(last), size - 1)) << strerror(errno);
    EXPECT_EQ(0, last) << "size=" << size;
  }

  ASSERT_EQ(0, close(fd)) << strerror(errno);
  ASSERT_EQ(0, unlink(path.c_str())) << strerror(errno);
}

TEST(FatConcurrentAllocation, FragmentedColdReadsSurviveTruncateRegrowthAndRename) {
  struct statfs fs_type {};
  ASSERT_EQ(0, statfs("/", &fs_type)) << strerror(errno);
  if (fs_type.f_type != kMsdosSuperMagic) {
    GTEST_SKIP() << "root filesystem is not FAT";
  }
  constexpr size_t kChunk = 64 * 1024;
  constexpr size_t kSize = 2 * 1024 * 1024;
  struct statvfs space {};
  ASSERT_EQ(0, statvfs("/", &space));
  if (space.f_bavail * space.f_frsize < 3 * kSize) {
    GTEST_SKIP() << "insufficient free FAT space";
  }
  TestFiles files;
  for (size_t i = 0; i < 2; ++i) {
    files.fds[i] = open(TestPath(i).c_str(), O_CREAT | O_EXCL | O_RDWR, 0600);
    ASSERT_GE(files.fds[i], 0) << strerror(errno);
  }
  std::vector<uint8_t> chunk(kChunk);
  // Alternate actual allocation between two files, rather than assuming that
  // writing a single file produces a fragmented chain. Sync each growth step.
  for (size_t offset = 0; offset < kSize; offset += kChunk) {
    for (size_t i = 0; i < chunk.size(); ++i) {
      chunk[i] = static_cast<uint8_t>(((offset + i) / kBlockSize) % 251 + 1);
    }
    ASSERT_TRUE(WriteAll(files.fds[0], chunk.data(), chunk.size()));
    ASSERT_EQ(0, fsync(files.fds[0])) << strerror(errno);
    std::fill(chunk.begin(), chunk.end(), 0xde);
    ASSERT_TRUE(WriteAll(files.fds[1], chunk.data(), chunk.size()));
    ASSERT_EQ(0, fsync(files.fds[1])) << strerror(errno);
  }
  auto check_cold = [&](int fd, bool reverse, size_t preserved) {
    ASSERT_EQ(0, fsync(fd)) << strerror(errno);
    ASSERT_EQ(0, posix_fadvise(fd, 0, 0, POSIX_FADV_RANDOM));
    ASSERT_EQ(0, posix_fadvise(fd, 0, 0, POSIX_FADV_DONTNEED));
    std::array<uint8_t, kBlockSize> block;
    for (size_t n = 0; n < kSize / kBlockSize; ++n) {
      size_t index = reverse ? kSize / kBlockSize - 1 - n : n;
      const size_t offset = index * kBlockSize;
      ASSERT_TRUE(ReadAllAt(fd, block.data(), block.size(), offset))
          << "offset=" << offset << ": " << strerror(errno);
      for (size_t i = 0; i < block.size(); ++i) {
        uint8_t expected = offset + i < preserved
                               ? static_cast<uint8_t>(index % 251 + 1) : 0;
        ASSERT_EQ(expected, block[i]) << "offset=" << offset + i;
      }
    }
  };
  ASSERT_NO_FATAL_FAILURE(check_cold(files.fds[0], false, kSize));
  ASSERT_NO_FATAL_FAILURE(check_cold(files.fds[0], true, kSize));
  // Leave the physical lookup cursor at the old tail before freeing it.
  ASSERT_EQ(0, posix_fadvise(files.fds[0], 0, 0, POSIX_FADV_DONTNEED));
  std::array<uint8_t, kBlockSize> tail;
  ASSERT_TRUE(ReadAllAt(files.fds[0], tail.data(), tail.size(), kSize - kBlockSize));
  constexpr size_t kRetained = 3 * kChunk + 123;
  ASSERT_EQ(0, ftruncate(files.fds[0], kRetained));
  ASSERT_EQ(0, fsync(files.fds[0]));
  // Reuse released space in another chain before growing the original again.
  std::fill(chunk.begin(), chunk.end(), 0xa7);
  ASSERT_TRUE(WriteAll(files.fds[1], chunk.data(), chunk.size()));
  ASSERT_EQ(0, fsync(files.fds[1]));
  ASSERT_EQ(0, ftruncate(files.fds[0], kSize)) << strerror(errno) << " errno=" << errno;
  ASSERT_NO_FATAL_FAILURE(check_cold(files.fds[0], false, kRetained));
  ASSERT_NO_FATAL_FAILURE(check_cold(files.fds[0], true, kRetained));
  ASSERT_EQ(0, rename(TestPath(0).c_str(), TestPath(2).c_str())) << strerror(errno);
  files.fds[2] = open(TestPath(2).c_str(), O_RDONLY);
  ASSERT_GE(files.fds[2], 0) << strerror(errno);
  ASSERT_NO_FATAL_FAILURE(check_cold(files.fds[2], false, kRetained));
}

void CheckRemovedOpenFile(bool replace_by_rename) {
  struct statfs fs_type {};
  ASSERT_EQ(0, statfs("/", &fs_type)) << strerror(errno);
  if (fs_type.f_type != kMsdosSuperMagic) {
    GTEST_SKIP() << "root filesystem is not FAT";
  }
  constexpr size_t kSize = 256 * 1024;
  struct statvfs space {};
  ASSERT_EQ(0, statvfs("/", &space));
  if (space.f_bavail * space.f_frsize < 4 * kSize + 1024 * 1024) {
    GTEST_SKIP() << "insufficient free FAT space";
  }
  TestFiles files;
  std::vector<uint8_t> old_data(kSize, 0x31);
  const std::vector<uint8_t> new_data(kSize, 0x62);
  files.fds[0] = open(TestPath(0).c_str(), O_CREAT | O_EXCL | O_RDWR, 0600);
  ASSERT_GE(files.fds[0], 0) << strerror(errno);
  ASSERT_TRUE(WriteAll(files.fds[0], old_data.data(), old_data.size()));
  ASSERT_EQ(0, fsync(files.fds[0]));
  ASSERT_EQ(0, posix_fadvise(files.fds[0], 0, 0, POSIX_FADV_DONTNEED));
  std::array<uint8_t, kBlockSize> tail;
  ASSERT_TRUE(ReadAllAt(files.fds[0], tail.data(), tail.size(), kSize - kBlockSize));
  // The old inode's physical read cursor now points into its last cluster.
  if (!replace_by_rename) {
    ASSERT_EQ(0, unlink(TestPath(0).c_str())) << strerror(errno);
  }
  files.fds[1] = open(TestPath(replace_by_rename ? 1 : 0).c_str(),
                      O_CREAT | O_EXCL | O_RDWR, 0600);
  ASSERT_GE(files.fds[1], 0) << strerror(errno);
  ASSERT_TRUE(WriteAll(files.fds[1], new_data.data(), new_data.size()));
  ASSERT_EQ(0, fsync(files.fds[1]));
  if (replace_by_rename) {
    ASSERT_EQ(0, rename(TestPath(1).c_str(), TestPath(0).c_str())) << strerror(errno);
  }
  files.fds[2] = open(TestPath(2).c_str(), O_CREAT | O_EXCL | O_RDWR, 0600);
  ASSERT_GE(files.fds[2], 0);
  std::vector<uint8_t> pressure(2 * kSize, 0xa7);
  ASSERT_TRUE(WriteAll(files.fds[2], pressure.data(), pressure.size()));
  ASSERT_EQ(0, fsync(files.fds[2]));
  auto check_cold = [&](int fd, const std::vector<uint8_t>& expected) {
    ASSERT_EQ(0, fsync(fd)) << strerror(errno);
    ASSERT_EQ(0, posix_fadvise(fd, 0, 0, POSIX_FADV_DONTNEED));
    std::vector<uint8_t> observed(expected.size());
    ASSERT_TRUE(ReadAllAt(fd, observed.data(), observed.size(), 0)) << strerror(errno);
    ASSERT_TRUE(observed == expected) << "cold content mismatch after namespace removal";
    struct stat st {};
    ASSERT_EQ(0, fstat(fd, &st));
    ASSERT_EQ(static_cast<off_t>(expected.size()), st.st_size);
  };
  // Check the cursor hit directly before a full cold scan could reset it.
  ASSERT_EQ(0, posix_fadvise(files.fds[0], 0, 0, POSIX_FADV_DONTNEED));
  ASSERT_TRUE(ReadAllAt(files.fds[0], tail.data(), tail.size(), kSize - kBlockSize));
  ASSERT_TRUE(std::all_of(tail.begin(), tail.end(), [](uint8_t b) { return b == 0x31; }));
  ASSERT_NO_FATAL_FAILURE(check_cold(files.fds[0], old_data));
  old_data[0] = 0x45;
  ASSERT_EQ(1, pwrite(files.fds[0], old_data.data(), 1, 0));
  std::array<uint8_t, kBlockSize> appended;
  appended.fill(0x56);
  ASSERT_EQ(static_cast<ssize_t>(appended.size()),
            pwrite(files.fds[0], appended.data(), appended.size(), old_data.size()));
  old_data.insert(old_data.end(), appended.begin(), appended.end());
  ASSERT_NO_FATAL_FAILURE(check_cold(files.fds[0], old_data));
  const size_t grown_size = old_data.size();
  constexpr size_t kShrunk = kSize / 2 + 123;
  ASSERT_EQ(0, ftruncate(files.fds[0], kShrunk)) << strerror(errno);
  old_data.resize(kShrunk);
  ASSERT_EQ(0, ftruncate(files.fds[0], grown_size)) << strerror(errno);
  old_data.resize(grown_size, 0);
  ASSERT_NO_FATAL_FAILURE(check_cold(files.fds[0], old_data));
  // Reopen by pathname: stale directory-entry writeback must not overwrite
  // the replacement file's size or cluster pointer.
  files.fds[3] = open(TestPath(0).c_str(), O_RDONLY);
  ASSERT_GE(files.fds[3], 0) << strerror(errno);
  ASSERT_NO_FATAL_FAILURE(check_cold(files.fds[3], new_data));
  ASSERT_NO_FATAL_FAILURE(check_cold(files.fds[2], pressure));
  // A mapping is another live reference to the removed inode. Release the
  // final fd first, then verify shared writes and delayed inode reclamation.
  struct MappedPage {
    void* address = MAP_FAILED;
    ~MappedPage() {
      if (address != MAP_FAILED) munmap(address, kBlockSize);
    }
  } mapping;
  mapping.address = mmap(nullptr, kBlockSize, PROT_READ | PROT_WRITE,
                         MAP_SHARED, files.fds[0], 0);
  ASSERT_NE(MAP_FAILED, mapping.address) << strerror(errno);
  struct statvfs before {}, after {};
  // Drain unrelated dirty allocations (including a freshly downloaded test
  // binary) before measuring reclamation. The mapping still pins this inode.
  ASSERT_EQ(0, syncfs(files.fds[3])) << strerror(errno);
  ASSERT_EQ(0, statvfs("/", &before));
  int closed = close(files.fds[0]);
  files.fds[0] = -1;
  ASSERT_EQ(0, closed);
  ASSERT_EQ(0, syncfs(files.fds[3])) << strerror(errno);
  auto* mapped = static_cast<volatile uint8_t*>(mapping.address);
  EXPECT_EQ(old_data[0], mapped[0]);
  mapped[3] = 0x7c;
  EXPECT_EQ(0x7c, mapped[3]);
  ASSERT_EQ(0, msync(mapping.address, kBlockSize, MS_SYNC)) << strerror(errno);
  ASSERT_NO_FATAL_FAILURE(check_cold(files.fds[3], new_data));
  ASSERT_EQ(0, munmap(mapping.address, kBlockSize));
  mapping.address = MAP_FAILED;
  ASSERT_EQ(0, syncfs(files.fds[3])) << strerror(errno);
  ASSERT_EQ(0, statvfs("/", &after));
  ASSERT_GE(after.f_bfree, before.f_bfree);
  EXPECT_GE((after.f_bfree - before.f_bfree) * after.f_frsize, old_data.size())
      << "final munmap plus syncfs must release the removed file's data clusters";
  ASSERT_NO_FATAL_FAILURE(check_cold(files.fds[3], new_data));
}

TEST(FatConcurrentAllocation, UnlinkedOpenFileRetainsPrivateChainUntilLastClose) {
  CheckRemovedOpenFile(false);
}

TEST(FatConcurrentAllocation, ReplacedOpenFileRetainsPrivateChainUntilLastClose) {
  CheckRemovedOpenFile(true);
}

}  // namespace

int main(int argc, char** argv) {
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
