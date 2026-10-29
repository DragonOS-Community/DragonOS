#include <gtest/gtest.h>

#include "loop_ext4_test_support.h"

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdint.h>
#include <sys/stat.h>
#include <unistd.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <string>
#include <thread>
#include <vector>

namespace {

using Clock = std::chrono::steady_clock;
// The 8 MiB fixture has about 3.4 MiB free after its journal and metadata.
constexpr size_t kTotalCalls = 4 * 4;
constexpr size_t kBytes = 128 * 1024;
// Leave holes, and cross unaligned page and 64 KiB user-copy boundaries.
constexpr size_t kStride = kBytes + 4096 + 17;
constexpr size_t kLeadingHole = 31;
constexpr off_t kPosition = 137;

struct ScopedFd {
    int value;
    ~ScopedFd() { if (value >= 0) close(value); }
    int Close() {
        const int fd = value;
        value = -1;
        return close(fd);
    }
};

struct WriterResult {
    int error = 0;
    size_t completed = 0;
    size_t short_count = 0;
    std::vector<uint64_t> syscall_ns;
    std::vector<uint64_t> operation_ns;
};

uint64_t Nanoseconds(Clock::duration duration) {
    return std::chrono::duration_cast<std::chrono::nanoseconds>(duration).count();
}

uint64_t Percentile(std::vector<uint64_t> samples, size_t percent) {
    std::sort(samples.begin(), samples.end());
    return samples[(samples.size() * percent + 99) / 100 - 1];
}

unsigned char Tag(size_t writer, size_t call) {
    return static_cast<unsigned char>(1 + writer * kTotalCalls + call);
}

size_t Offset(size_t writer, size_t call, size_t writers, bool separate) {
    return kLeadingHole + (separate ? call : call * writers + writer) * kStride;
}

void WriteAll(int fd, const std::vector<unsigned char>& bytes, off_t offset) {
    size_t done = 0;
    while (done < bytes.size()) {
        ssize_t count = pwrite(fd, bytes.data() + done, bytes.size() - done, offset + done);
        if (count < 0 && errno == EINTR) continue;
        ASSERT_GT(count, 0) << strerror(errno);
        ASSERT_LE(static_cast<size_t>(count), bytes.size() - done);
        done += count;
    }
}

// A cost regression test records partial writes without treating Linux's
// permitted positive prefix as a failure. Timings are diagnostics, never a
// machine-dependent pass threshold. Content, size and f_pos remain strict.
void RunProgress(const char* name, size_t writers, bool separate, bool overwrite) {
    dunitest::LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp("ext4_inode_identity.img", "/tmp", name));
    ASSERT_NO_FATAL_FAILURE(fs.Mount());
    const size_t calls = kTotalCalls / writers;
    const size_t files = separate ? writers : 1;
    const size_t slots = separate ? calls : kTotalCalls;
    const size_t data_end = kLeadingHole + (slots - 1) * kStride + kBytes;
    std::vector<std::string> paths;
    for (size_t file = 0; file < files; ++file) {
        paths.push_back(fs.mount_point() + "/progress_" + std::to_string(file));
        ScopedFd descriptor{open(paths.back().c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)};
        const int fd = descriptor.value;
        ASSERT_GE(fd, 0) << strerror(errno);
        if (overwrite) {
            // Materialize blocks before timing; ftruncate would leave sparse
            // holes and would not isolate already-allocated overwrite cost.
            std::vector<unsigned char> zero(kBytes, 0);
            for (size_t done = 0; done < data_end;) {
                zero.resize(std::min(kBytes, data_end - done));
                ASSERT_NO_FATAL_FAILURE(WriteAll(fd, zero, done));
                done += zero.size();
            }
            ASSERT_EQ(0, fsync(fd)) << strerror(errno);
        }
        ASSERT_EQ(0, descriptor.Close()) << strerror(errno);
    }

    std::vector<WriterResult> results(writers);
    std::atomic<size_t> ready{0};
    std::atomic<bool> start{false};
    std::vector<std::thread> workers;
    for (size_t writer = 0; writer < writers; ++writer) {
        workers.emplace_back([&, writer] {
            auto& result = results[writer];
            // Each worker has its own OFD: no shared-position lock can hide
            // concurrent changes to the same inode's delayed-write admission.
            int fd = open(paths[separate ? writer : 0].c_str(), O_RDWR);
            if (fd < 0) result.error = errno;
            if (fd >= 0 && lseek(fd, kPosition, SEEK_SET) != kPosition) result.error = errno ? errno : EIO;
            std::vector<unsigned char> bytes(kBytes);
            result.syscall_ns.reserve(calls);
            result.operation_ns.reserve(calls);
            ready.fetch_add(1, std::memory_order_release);
            while (!start.load(std::memory_order_acquire)) std::this_thread::yield();
            for (size_t call = 0; call < calls && result.error == 0; ++call) {
                std::fill(bytes.begin(), bytes.end(), Tag(writer, call));
                size_t done = 0;
                const auto operation_start = Clock::now();
                while (done < bytes.size()) {
                    const size_t remaining = bytes.size() - done;
                    const auto syscall_start = Clock::now();
                    ssize_t count = pwrite(fd, bytes.data() + done, remaining,
                                          Offset(writer, call, writers, separate) + done);
                    const int error = errno;
                    result.syscall_ns.push_back(Nanoseconds(Clock::now() - syscall_start));
                    if (count < 0 && error == EINTR) continue;
                    if (count <= 0 || static_cast<size_t>(count) > remaining) {
                        result.error = count < 0 ? error : EIO;
                        break;
                    }
                    if (static_cast<size_t>(count) < remaining) ++result.short_count;
                    done += count;
                    result.completed += count;
                }
                result.operation_ns.push_back(Nanoseconds(Clock::now() - operation_start));
            }
            if (fd >= 0) {
                if (lseek(fd, 0, SEEK_CUR) != kPosition && result.error == 0) result.error = EIO;
                if (close(fd) != 0 && result.error == 0) result.error = errno;
            }
        });
    }
    while (ready.load(std::memory_order_acquire) != writers) std::this_thread::yield();
    const auto begin = Clock::now();
    start.store(true, std::memory_order_release);
    for (auto& worker : workers) worker.join();
    const auto elapsed = Nanoseconds(Clock::now() - begin);

    size_t completed = 0, short_count = 0;
    std::vector<uint64_t> syscall_ns, operation_ns;
    for (size_t writer = 0; writer < writers; ++writer) {
        const auto& result = results[writer];
        ASSERT_EQ(0, result.error) << "writer=" << writer << ": " << strerror(result.error);
        ASSERT_EQ(calls * kBytes, result.completed);
        completed += result.completed;
        short_count += result.short_count;
        syscall_ns.insert(syscall_ns.end(), result.syscall_ns.begin(), result.syscall_ns.end());
        operation_ns.insert(operation_ns.end(), result.operation_ns.begin(), result.operation_ns.end());
    }
    const double mib = static_cast<double>(completed) / (1024 * 1024);
    printf("[ EXT4_PROGRESS ] mode=%s bytes=%zu short_count=%zu syscalls=%zu "
           "syscalls_per_mib=%.3f elapsed_ns=%llu mib_per_second=%.3f "
           "syscall_p50_ns=%llu syscall_p95_ns=%llu syscall_p99_ns=%llu "
           "operation_p50_ns=%llu operation_p95_ns=%llu operation_p99_ns=%llu\n",
           name, completed, short_count, syscall_ns.size(), syscall_ns.size() / mib,
           static_cast<unsigned long long>(elapsed), elapsed ? mib * 1e9 / elapsed : 0,
           static_cast<unsigned long long>(Percentile(syscall_ns, 50)),
           static_cast<unsigned long long>(Percentile(syscall_ns, 95)),
           static_cast<unsigned long long>(Percentile(syscall_ns, 99)),
           static_cast<unsigned long long>(Percentile(operation_ns, 50)),
           static_cast<unsigned long long>(Percentile(operation_ns, 95)),
           static_cast<unsigned long long>(Percentile(operation_ns, 99)));

    std::vector<std::vector<unsigned char>> expected(files,
                                                  std::vector<unsigned char>(data_end + 24, 0));
    for (size_t writer = 0; writer < writers; ++writer) {
        for (size_t call = 0; call < calls; ++call) {
            auto& image = expected[separate ? writer : 0];
            const auto offset = Offset(writer, call, writers, separate);
            std::fill(image.begin() + offset, image.begin() + offset + kBytes, Tag(writer, call));
        }
    }
    for (size_t file = 0; file < files; ++file) {
        ScopedFd descriptor{open(paths[file].c_str(), O_RDWR)};
        const int fd = descriptor.value;
        ASSERT_GE(fd, 0) << strerror(errno);
        struct stat st = {};
        ASSERT_EQ(0, fstat(fd, &st)) << strerror(errno);
        EXPECT_EQ(static_cast<off_t>(data_end), st.st_size);
        // Publish a one-byte tail past a fresh hole, then check it survives
        // fsync/remount together with all submitted prefixes and old holes.
        const std::vector<unsigned char> tail{0xd7};
        ASSERT_NO_FATAL_FAILURE(WriteAll(fd, tail, data_end + 23));
        expected[file].back() = tail.front();
        ASSERT_EQ(0, fsync(fd)) << strerror(errno);
        ASSERT_EQ(0, descriptor.Close()) << strerror(errno);
    }
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());
    for (size_t file = 0; file < files; ++file) {
        ScopedFd descriptor{open(paths[file].c_str(), O_RDONLY)};
        const int fd = descriptor.value;
        ASSERT_GE(fd, 0) << strerror(errno);
        struct stat st = {};
        ASSERT_EQ(0, fstat(fd, &st)) << strerror(errno);
        ASSERT_EQ(static_cast<off_t>(expected[file].size()), st.st_size);
        std::vector<unsigned char> observed(expected[file].size());
        size_t done = 0;
        while (done < observed.size()) {
            ssize_t count = pread(fd, observed.data() + done, observed.size() - done, done);
            if (count < 0 && errno == EINTR) continue;
            ASSERT_GT(count, 0) << strerror(errno);
            ASSERT_LE(static_cast<size_t>(count), observed.size() - done);
            done += count;
        }
        const auto mismatch = std::mismatch(expected[file].begin(), expected[file].end(), observed.begin());
        EXPECT_EQ(expected[file].end(), mismatch.first)
            << "file=" << file << " byte=" << (mismatch.first - expected[file].begin());
        unsigned char byte = 0;
        EXPECT_EQ(0, pread(fd, &byte, 1, observed.size()));
        EXPECT_EQ(0, lseek(fd, 0, SEEK_CUR));
        ASSERT_EQ(0, descriptor.Close()) << strerror(errno);
    }
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

TEST(Ext4WriteProgress, ConcurrentExtendingWrites) {
    RunProgress("ext4_progress_concurrent", 4, false, false);
}

TEST(Ext4WriteProgress, SingleWriterExtendingControl) {
    RunProgress("ext4_progress_single", 1, false, false);
}

TEST(Ext4WriteProgress, PreparedOverwriteControl) {
    RunProgress("ext4_progress_overwrite", 4, false, true);
}

TEST(Ext4WriteProgress, SeparateInodeControl) {
    RunProgress("ext4_progress_separate", 4, true, false);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
