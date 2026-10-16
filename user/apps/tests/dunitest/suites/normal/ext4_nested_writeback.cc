#include "loop_ext4_test_support.h"

#include <array>
#include <atomic>
#include <thread>
#include <vector>

namespace {

constexpr size_t kPageSize = 4096;
constexpr size_t kPages = 128;
constexpr size_t kWriters = 4;

// Keep the outer device on tmpfs even when a caller has bind-mounted ext4
// over /tmp. Only the inner loop device must depend on ext4 writeback.
class TmpfsBacking {
  public:
    ~TmpfsBacking() {
        if (mounted_) {
            umount(path_.c_str());
        }
        if (!path_.empty()) {
            rmdir(path_.c_str());
        }
    }

    void SetUp() {
        path_ = "/tmp/ext4_nested_backing_" + std::to_string(getpid());
        ASSERT_EQ(0, mkdir(path_.c_str(), 0700)) << strerror(errno);
        ASSERT_EQ(0, mount("tmpfs", path_.c_str(), "tmpfs", 0, nullptr))
            << strerror(errno);
        mounted_ = true;
    }

    const std::string& path() const {
        return path_;
    }

  private:
    std::string path_;
    bool mounted_ = false;
};

void FillPage(std::array<unsigned char, kPageSize>& page, unsigned seed, size_t index) {
    for (size_t i = 0; i < page.size(); ++i) {
        page[i] = static_cast<unsigned char>(seed + index * 7 + i % 251);
    }
}

void RecordError(std::atomic<int>& error, int value) {
    int expected = 0;
    error.compare_exchange_strong(expected, value ? value : EIO);
}

// No fatal gtest assertions in workers: every thread must join before the
// mount owners can be destroyed, including when an I/O operation fails.
void WritePages(const std::string& path, unsigned seed, size_t first_page,
                int flags, bool append, std::atomic<int>& error) {
    int fd = open(path.c_str(), flags, 0600);
    if (fd < 0) {
        RecordError(error, errno);
        return;
    }
    std::array<unsigned char, kPageSize> page;
    for (size_t index = 0; index < kPages && error.load() == 0; ++index) {
        FillPage(page, seed, index);
        size_t done = 0;
        while (done < page.size()) {
            ssize_t count = append
                ? write(fd, page.data() + done, page.size() - done)
                : pwrite(fd, page.data() + done, page.size() - done,
                         (first_page + index) * kPageSize + done);
            if (count <= 0) {
                RecordError(error, count < 0 ? errno : EIO);
                break;
            }
            done += static_cast<size_t>(count);
        }
        if ((index + 1) % 16 == 0 && fsync(fd) != 0) {
            RecordError(error, errno);
        }
    }
    if (fsync(fd) != 0) {
        RecordError(error, errno);
    }
    if (close(fd) != 0) {
        RecordError(error, errno);
    }
}

void VerifyFile(const std::string& path, unsigned first_seed,
                unsigned second_seed = 0, bool mixed = false) {
    int fd = open(path.c_str(), O_RDONLY);
    ASSERT_GE(fd, 0) << path << ": " << strerror(errno);
    struct stat attr = {};
    int stat_result = fstat(fd, &attr);
    EXPECT_EQ(0, stat_result) << strerror(errno);
    const size_t pages = mixed ? 2 * kPages : kPages;
    EXPECT_EQ(static_cast<off_t>(pages * kPageSize), attr.st_size) << path;
    bool contents_match = stat_result == 0;
    std::array<unsigned char, kPageSize> actual;
    std::array<unsigned char, kPageSize> expected;
    for (size_t index = 0; index < pages && contents_match; ++index) {
        FillPage(expected, index < kPages ? first_seed : second_seed, index % kPages);
        size_t done = 0;
        while (done < actual.size()) {
            ssize_t count = pread(fd, actual.data() + done, actual.size() - done,
                                  index * kPageSize + done);
            if (count <= 0) {
                contents_match = false;
                break;
            }
            done += static_cast<size_t>(count);
        }
        if (contents_match && actual != expected) {
            contents_match = false;
        }
        EXPECT_TRUE(contents_match) << path << ": page " << index;
    }
    EXPECT_EQ(0, close(fd)) << strerror(errno);
    ASSERT_TRUE(contents_match) << path;
}

TEST(Ext4NestedWriteback, ConcurrentAppendAndOverwriteSurviveRemount) {
    TmpfsBacking backing;
    ASSERT_NO_FATAL_FAILURE(backing.SetUp());
    dunitest::LoopExt4 outer;
    ASSERT_NO_FATAL_FAILURE(outer.SetUp("ext4_uninit_bitmap.img", backing.path().c_str(),
                                      "ext4_nested_outer"));
    ASSERT_NO_FATAL_FAILURE(outer.Mount());
    const std::string marker = outer.mount_point() + "/outer-marker";
    std::atomic<int> error{0};
    WritePages(marker, 0x41, 0, O_CREAT | O_EXCL | O_WRONLY, false, error);
    ASSERT_EQ(0, error.load()) << strerror(error.load());
    {
        dunitest::LoopExt4 inner;
        ASSERT_NO_FATAL_FAILURE(inner.SetUp("ext4_inode_identity.img",
                                          outer.mount_point().c_str(), "ext4_nested_inner"));
        ASSERT_NO_FATAL_FAILURE(inner.Mount());
        std::atomic<bool> start{false};
        std::vector<std::thread> writers;
        for (size_t writer = 0; writer < kWriters; ++writer) {
            writers.emplace_back([&, writer] {
                while (!start.load(std::memory_order_acquire)) {
                    std::this_thread::yield();
                }
                WritePages(inner.mount_point() + "/writer-" + std::to_string(writer),
                           0x20 + writer * 37, 0, O_CREAT | O_EXCL | O_WRONLY | O_APPEND,
                           true, error);
            });
        }
        start.store(true, std::memory_order_release);
        for (auto& writer : writers) {
            writer.join();
        }
        ASSERT_EQ(0, error.load()) << strerror(error.load());

        const std::string mixed = inner.mount_point() + "/mixed";
        WritePages(mixed, 0x53, 0, O_CREAT | O_EXCL | O_WRONLY, false, error);
        ASSERT_EQ(0, error.load()) << strerror(error.load());
        start.store(false, std::memory_order_release);
        std::thread appender([&] {
            while (!start.load(std::memory_order_acquire)) {
                std::this_thread::yield();
            }
            WritePages(mixed, 0x91, kPages, O_WRONLY | O_APPEND, true, error);
        });
        std::thread overwriter([&] {
            while (!start.load(std::memory_order_acquire)) {
                std::this_thread::yield();
            }
            WritePages(mixed, 0xc7, 0, O_WRONLY, false, error);
        });
        start.store(true, std::memory_order_release);
        appender.join();
        overwriter.join();
        ASSERT_EQ(0, error.load()) << strerror(error.load());

        for (int mount = 0; mount < 2; ++mount) {
            for (size_t writer = 0; writer < kWriters; ++writer) {
                ASSERT_NO_FATAL_FAILURE(VerifyFile(
                    inner.mount_point() + "/writer-" + std::to_string(writer),
                    0x20 + writer * 37));
            }
            ASSERT_NO_FATAL_FAILURE(VerifyFile(mixed, 0xc7, 0x91, true));
            ASSERT_NO_FATAL_FAILURE(inner.Unmount());
            if (mount == 0) {
                ASSERT_NO_FATAL_FAILURE(inner.Mount());
            }
        }
    }
    // Destroy the inner loop before unmounting the outer backing filesystem.
    ASSERT_NO_FATAL_FAILURE(outer.Unmount());
    ASSERT_NO_FATAL_FAILURE(outer.Mount());
    ASSERT_NO_FATAL_FAILURE(VerifyFile(marker, 0x41));
    ASSERT_NO_FATAL_FAILURE(outer.Unmount());
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
