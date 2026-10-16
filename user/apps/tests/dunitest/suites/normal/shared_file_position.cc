#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/eventfd.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <stdio.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <atomic>
#include <string>
#include <thread>
#include <vector>

namespace {
constexpr size_t kThreads = 4;
constexpr size_t kCalls = 4;
// Exercise both the 64 KiB backing-read boundary and multiple iovecs.
constexpr size_t kWords = 256 * 1024 / sizeof(uint64_t);
constexpr size_t kBytes = kWords * sizeof(uint64_t);
constexpr size_t kFileWords = kThreads * kCalls * kWords;

class SharedFilePosition : public ::testing::Test {
  protected:
    void SetUp() override {
        // Also bound a regression that deadlocks while acquiring an OFD lock.
        alarm(30);
        fd = mkstemp(path);
        ASSERT_GE(fd, 0);
        for (auto& copy : copies) {
            copy = dup(fd);
            ASSERT_GE(copy, 0);
        }
    }
    void TearDown() override {
        alarm(0);
        for (int copy : copies) if (copy >= 0) close(copy);
        if (fd >= 0) close(fd);
        unlink(path);
    }
    void FillNumberedFile() {
        std::vector<uint64_t> data(kFileWords);
        for (size_t i = 0; i < data.size(); ++i) data[i] = i;
        ASSERT_EQ(static_cast<ssize_t>(data.size() * sizeof(uint64_t)),
                  write(fd, data.data(), data.size() * sizeof(uint64_t)));
        ASSERT_EQ(0, lseek(fd, 0, SEEK_SET));
    }
    template <typename Function>
    void Concurrent(Function function) {
        std::atomic<size_t> ready{0};
        std::atomic<bool> start{false};
        std::array<std::thread, kThreads> workers;
        for (size_t i = 0; i < kThreads; ++i) {
            workers[i] = std::thread([&, i] {
                ready.fetch_add(1, std::memory_order_release);
                while (!start.load(std::memory_order_acquire)) sched_yield();
                function(i);
            });
        }
        while (ready.load(std::memory_order_acquire) != kThreads) sched_yield();
        start.store(true, std::memory_order_release);
        for (auto& worker : workers) worker.join();
    }
    void CheckConcurrentReads(int operation) {
        ASSERT_NO_FATAL_FAILURE(FillNumberedFile());
        std::array<std::array<uint64_t, kCalls>, kThreads> starts{};
        std::array<bool, kThreads> valid;
        valid.fill(true);
        Concurrent([&](size_t thread) {
            std::vector<uint64_t> data(kWords);
            for (size_t call = 0; call < kCalls; ++call) {
                iovec iovs[] = {{data.data(), kBytes / 8},
                               {data.data() + kWords / 8, kBytes / 2},
                               {data.data() + kWords * 5 / 8, kBytes * 3 / 8}};
                ssize_t count;
                if (operation == 0) count = read(copies[thread], data.data(), kBytes);
                else if (operation == 1) count = readv(copies[thread], iovs, 3);
                else {
#if defined(__x86_64__)
                    count = syscall(SYS_preadv2, copies[thread], iovs, 3, -1L, -1L, 0L);
#else
                    count = syscall(SYS_preadv2, copies[thread], iovs, 3, -1L, 0L);
#endif
                }
                if (count != static_cast<ssize_t>(kBytes)) {
                    valid[thread] = false;
                    continue;
                }
                starts[thread][call] = data.front();
                for (size_t word = 0; word < data.size(); ++word) {
                    if (data[word] != data.front() + word) valid[thread] = false;
                }
            }
        });
        std::vector<uint64_t> ordered;
        for (size_t thread = 0; thread < kThreads; ++thread) {
            EXPECT_TRUE(valid[thread]) << "noncontiguous or short syscall, thread " << thread;
            ordered.insert(ordered.end(), starts[thread].begin(), starts[thread].end());
        }
        std::sort(ordered.begin(), ordered.end());
        for (size_t i = 0; i < ordered.size(); ++i) EXPECT_EQ(i * kWords, ordered[i]);
        EXPECT_EQ(static_cast<off_t>(kFileWords * sizeof(uint64_t)), lseek(fd, 0, SEEK_CUR));
    }
    void CheckConcurrentWrites(bool vectored, bool current_pwritev2 = false, int flags = 0) {
        std::array<bool, kThreads> valid;
        valid.fill(true);
        Concurrent([&](size_t thread) {
            std::vector<uint64_t> data(kWords);
            for (size_t call = 0; call < kCalls; ++call) {
                std::fill(data.begin(), data.end(), thread * kCalls + call + 1);
                iovec iovs[] = {{data.data(), kBytes / 2},
                               {data.data() + kWords / 2, kBytes / 2}};
                ssize_t count;
                if (current_pwritev2 && thread % 2 == 0) {
                    #if defined(__x86_64__)
                    count = syscall(SYS_pwritev2, copies[thread], iovs, 2, -1L, -1L, flags);
#else
                    count = syscall(SYS_pwritev2, copies[thread], iovs, 2, -1L, flags);
#endif
                } else {
                    count = vectored ? writev(copies[thread], iovs, 2)
                                     : write(copies[thread], data.data(), kBytes);
                }
                if (count != static_cast<ssize_t>(kBytes)) valid[thread] = false;
            }
        });
        for (bool success : valid) EXPECT_TRUE(success);
        EXPECT_EQ(static_cast<off_t>(kFileWords * sizeof(uint64_t)), lseek(fd, 0, SEEK_CUR));
        struct stat st;
        ASSERT_EQ(0, fstat(fd, &st));
        ASSERT_EQ(static_cast<off_t>(kFileWords * sizeof(uint64_t)), st.st_size);
        std::vector<uint64_t> data(kFileWords);
        ASSERT_EQ(st.st_size, pread(fd, data.data(), st.st_size, 0));
        std::array<size_t, kThreads * kCalls + 1> occurrences{};
        for (size_t block = 0; block < kThreads * kCalls; ++block) {
            uint64_t tag = data[block * kWords];
            ASSERT_GT(tag, 0u);
            ASSERT_LT(tag, occurrences.size());
            ++occurrences[tag];
            for (size_t word = 0; word < kWords; ++word) {
                ASSERT_EQ(tag, data[block * kWords + word]) << "interleaved write at block " << block;
            }
        }
        for (size_t tag = 1; tag < occurrences.size(); ++tag) EXPECT_EQ(1u, occurrences[tag]);
    }
    char path[sizeof("/tmp/dunitest_shared_position_XXXXXX")] =
        "/tmp/dunitest_shared_position_XXXXXX";
    int fd = -1;
    std::array<int, kThreads> copies{{-1, -1, -1, -1}};
};

TEST_F(SharedFilePosition, ConcurrentReadCallsAreContiguousAndDisjoint) {
    CheckConcurrentReads(0);
}
TEST_F(SharedFilePosition, ConcurrentReadvCallsAreContiguousAndDisjoint) {
    CheckConcurrentReads(1);
}
TEST_F(SharedFilePosition, ConcurrentPreadv2CurrentPositionCallsAreContiguousAndDisjoint) {
    CheckConcurrentReads(2);
}
TEST_F(SharedFilePosition, ConcurrentWriteCallsRemainWhole) {
    CheckConcurrentWrites(false);
}
TEST_F(SharedFilePosition, ConcurrentWritevCallsRemainWhole) {
    CheckConcurrentWrites(true);
}

TEST_F(SharedFilePosition, SeekingCannotSplitAReadCall) {
    ASSERT_NO_FATAL_FAILURE(FillNumberedFile());
    std::array<bool, kThreads> valid;
    valid.fill(true);
    Concurrent([&](size_t thread) {
        if (thread == 0) {
            for (size_t i = 0; i < 512; ++i) {
                if (lseek(copies[thread], 0, SEEK_SET) != 0) valid[thread] = false;
                sched_yield();
            }
            return;
        }
        std::vector<uint64_t> data(kWords);
        for (size_t call = 0; call < kCalls; ++call) {
            ssize_t count = read(copies[thread], data.data(), kBytes);
            if (count != static_cast<ssize_t>(kBytes)) {
                valid[thread] = false;
                continue;
            }
            for (size_t word = 0; word < data.size(); ++word) {
                if (data[word] != data.front() + word) valid[thread] = false;
            }
        }
    });
    for (bool success : valid) EXPECT_TRUE(success);
}

TEST_F(SharedFilePosition, RelativeSeekAndWriteDoNotLosePositionUpdates) {
    constexpr size_t iterations = 512;
    std::array<bool, kThreads> valid;
    valid.fill(true);
    Concurrent([&](size_t thread) {
        for (size_t i = 0; i < iterations; ++i) {
            if (thread % 2 == 0) {
                if (lseek(copies[thread], 1, SEEK_CUR) < 0) valid[thread] = false;
            } else {
                char byte = 'w';
                if (write(copies[thread], &byte, 1) != 1) valid[thread] = false;
            }
        }
    });
    for (bool success : valid) EXPECT_TRUE(success);
    EXPECT_EQ(static_cast<off_t>(kThreads * iterations), lseek(fd, 0, SEEK_CUR));
}

TEST_F(SharedFilePosition, PositionalIoDoesNotChangeSharedPosition) {
    ASSERT_EQ(137, lseek(fd, 137, SEEK_SET));
    struct Result {
        ssize_t written = -1;
        ssize_t read = -1;
        int write_error = 0;
        int read_error = 0;
        off_t size_after_write = -1;
        size_t bad_byte = kBytes;
        unsigned char actual = 0;
    };
    std::array<std::array<Result, kCalls>, kThreads> results{};
    Concurrent([&](size_t thread) {
        std::vector<uint64_t> written(kWords, thread + 1), read_back(kWords);
        off_t position = thread * kBytes;
        for (size_t call = 0; call < kCalls; ++call) {
            auto& result = results[thread][call];
            errno = 0;
            result.written = pwrite(copies[thread], written.data(), kBytes, position);
            result.write_error = errno;
            struct stat st;
            if (fstat(copies[thread], &st) == 0) result.size_after_write = st.st_size;
            std::fill(read_back.begin(), read_back.end(), UINT64_MAX);
            errno = 0;
            // A regular-file pwrite may succeed with a positive prefix. Verify
            // exactly that committed prefix without retrying or assuming the
            // unwritten remainder of an extending write contains our data.
            size_t committed = result.written > 0 && result.written <= static_cast<ssize_t>(kBytes)
                                   ? static_cast<size_t>(result.written) : 0;
            result.read = pread(copies[thread], read_back.data(), committed, position);
            result.read_error = errno;
            if (result.read == static_cast<ssize_t>(committed)) {
                const auto* expected = reinterpret_cast<const unsigned char*>(written.data());
                const auto* actual = reinterpret_cast<const unsigned char*>(read_back.data());
                for (size_t byte = 0; byte < committed; ++byte) {
                    if (actual[byte] != expected[byte]) {
                        result.bad_byte = byte;
                        result.actual = actual[byte];
                        break;
                    }
                }
            }
        }
    });
    for (size_t thread = 0; thread < kThreads; ++thread) {
        for (size_t call = 0; call < kCalls; ++call) {
            SCOPED_TRACE(::testing::Message() << "thread=" << thread << " call=" << call
                         << " offset=" << thread * kBytes);
            const auto& result = results[thread][call];
            EXPECT_GT(result.written, 0)
                << "pwrite errno=" << result.write_error << " size=" << result.size_after_write;
            EXPECT_LE(result.written, static_cast<ssize_t>(kBytes));
            EXPECT_EQ(result.written, result.read)
                << "pread errno=" << result.read_error << " size=" << result.size_after_write;
            uint64_t expected = thread + 1;
            const auto* expected_bytes = reinterpret_cast<const unsigned char*>(&expected);
            EXPECT_EQ(kBytes, result.bad_byte)
                << "first mismatching byte=" << result.bad_byte
                << " actual=" << static_cast<unsigned>(result.actual)
                << " expected=" << static_cast<unsigned>(expected_bytes[result.bad_byte % sizeof(expected)])
                << " size=" << result.size_after_write;
        }
    }
    EXPECT_EQ(137, lseek(fd, 0, SEEK_CUR));
}

TEST_F(SharedFilePosition, ForkSharesTheSamePositionTransaction) {
    ASSERT_NO_FATAL_FAILURE(FillNumberedFile());
    int results[2];
    ASSERT_EQ(0, pipe(results));
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(results[0]);
        std::vector<uint64_t> data(kWords);
        uint64_t report[2] = {0, 0};
        if (read(fd, data.data(), kBytes) == static_cast<ssize_t>(kBytes)) {
            report[0] = data.front();
            report[1] = 1;
            for (size_t i = 0; i < data.size(); ++i) {
                if (data[i] != data.front() + i) report[1] = 0;
            }
        }
        ssize_t count = write(results[1], report, sizeof(report));
        _exit(count == sizeof(report) && report[1] == 1 ? 0 : 1);
    }
    close(results[1]);
    std::vector<uint64_t> data(kWords);
    EXPECT_EQ(static_cast<ssize_t>(kBytes), read(fd, data.data(), kBytes));
    bool contiguous = true;
    for (size_t i = 0; i < data.size(); ++i) {
        if (data[i] != data.front() + i) contiguous = false;
    }
    EXPECT_TRUE(contiguous);
    uint64_t report[2] = {};
    EXPECT_EQ(static_cast<ssize_t>(sizeof(report)), read(results[0], report, sizeof(report)));
    close(results[0]);
    int status = 0;
    EXPECT_EQ(child, waitpid(child, &status, 0));
    EXPECT_TRUE(WIFEXITED(status) && WEXITSTATUS(status) == 0);
    EXPECT_EQ(1u, report[1]);
    EXPECT_EQ(0u, std::min(data.front(), report[0]));
    EXPECT_EQ(kWords, std::max(data.front(), report[0]));
    EXPECT_EQ(static_cast<off_t>(kBytes * 2), lseek(fd, 0, SEEK_CUR));
}

TEST_F(SharedFilePosition, SeparateOpenHasAnIndependentPosition) {
    ASSERT_NO_FATAL_FAILURE(FillNumberedFile());
    int other = open(path, O_RDONLY);
    ASSERT_GE(other, 0);
    uint64_t first = UINT64_MAX;
    EXPECT_EQ(static_cast<ssize_t>(sizeof(first)), read(fd, &first, sizeof(first)));
    EXPECT_EQ(0u, first);
    EXPECT_EQ(0, lseek(other, 0, SEEK_CUR));
    EXPECT_EQ(static_cast<ssize_t>(sizeof(first)), read(other, &first, sizeof(first)));
    EXPECT_EQ(0u, first);
    EXPECT_EQ(static_cast<off_t>(sizeof(first)), lseek(fd, 0, SEEK_CUR));
    close(other);
}

TEST_F(SharedFilePosition, SharedAppendWritersKeepCompleteCalls) {
    ASSERT_EQ(0, fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) | O_APPEND));
    CheckConcurrentWrites(true);
}

TEST_F(SharedFilePosition, Pwritev2CurrentPositionSharesOrdinaryWriteTransactions) {
    CheckConcurrentWrites(false, true);
}

TEST_F(SharedFilePosition, Pwritev2CurrentPositionSharesAppendWriteTransactions) {
    CheckConcurrentWrites(true, true, RWF_APPEND);
    ASSERT_EQ(0, lseek(fd, 0, SEEK_SET));
    uint64_t tag = 42;
    iovec iov{&tag, sizeof(tag)};
    ASSERT_EQ(static_cast<ssize_t>(sizeof(tag)), pwritev2(fd, &iov, 1, -1, RWF_APPEND));
    EXPECT_EQ(static_cast<off_t>(kFileWords * sizeof(uint64_t) + sizeof(tag)),
              lseek(fd, 0, SEEK_CUR));
    uint64_t tail = 0;
    ASSERT_EQ(static_cast<ssize_t>(sizeof(tail)),
              pread(fd, &tail, sizeof(tail), kFileWords * sizeof(uint64_t)));
    EXPECT_EQ(tag, tail);
}

TEST_F(SharedFilePosition, BlockingStreamReadAllowsSameOfdWrite) {
    int event = eventfd(0, 0);
    ASSERT_GE(event, 0);
    std::atomic<bool> entering{false};
    ssize_t received = -1;
    uint64_t value = 0;
    std::thread reader([&] {
        entering.store(true, std::memory_order_release);
        received = read(event, &value, sizeof(value));
    });
    while (!entering.load(std::memory_order_acquire)) sched_yield();
    usleep(20000);
    uint64_t sent = 2;
    EXPECT_EQ(static_cast<ssize_t>(sizeof(sent)), write(event, &sent, sizeof(sent)));
    reader.join();
    EXPECT_EQ(static_cast<ssize_t>(sizeof(value)), received);
    EXPECT_EQ(sent, value);
    close(event);
}

TEST_F(SharedFilePosition, CopyFileRangeSameOfdRejectsImplicitOverlap) {
    ASSERT_NO_FATAL_FAILURE(FillNumberedFile());
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_copy_file_range, fd, nullptr, copies[0], nullptr, 16, 0));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(0, lseek(fd, 0, SEEK_CUR));
}

TEST_F(SharedFilePosition, CopyFileRangeExplicitEndpointPreservesSharedPosition) {
    ASSERT_NO_FATAL_FAILURE(FillNumberedFile());
    off_t input = 4096;
    ASSERT_EQ(16, syscall(SYS_copy_file_range, fd, &input, copies[0], nullptr, 16, 0));
    EXPECT_EQ(4112, input);
    EXPECT_EQ(16, lseek(fd, 0, SEEK_CUR));
    // Conversely, an explicit output position does not commit over input f_pos.
    off_t output = 8192;
    ASSERT_EQ(16, syscall(SYS_copy_file_range, fd, nullptr, copies[0], &output, 16, 0));
    EXPECT_EQ(8208, output);
    EXPECT_EQ(32, lseek(fd, 0, SEEK_CUR));
    uint64_t first = UINT64_MAX;
    ASSERT_EQ(static_cast<ssize_t>(sizeof(first)), pread(fd, &first, sizeof(first), 0));
    EXPECT_EQ(4096 / sizeof(uint64_t), first);
}

TEST_F(SharedFilePosition, CopyFileRangeCanRaceWithOrdinaryIoWithoutDeadlock) {
    ASSERT_NO_FATAL_FAILURE(FillNumberedFile());
    char target_path[] = "/tmp/dunitest_position_target_XXXXXX";
    int target = mkstemp(target_path);
    ASSERT_GE(target, 0);
    ASSERT_EQ(0, unlink(target_path));
    std::array<bool, kThreads> valid;
    valid.fill(true);
    Concurrent([&](size_t thread) {
        std::array<char, 4096> bytes{};
        for (size_t call = 0; call < 32; ++call) {
            ssize_t count;
            if (thread < 2) {
                count = syscall(SYS_copy_file_range, fd, nullptr, target, nullptr,
                                bytes.size(), 0);
            } else if (thread == 2) {
                count = read(fd, bytes.data(), bytes.size());
            } else {
                count = write(target, bytes.data(), bytes.size());
            }
            if (count != static_cast<ssize_t>(bytes.size())) valid[thread] = false;
        }
    });
    for (bool success : valid) EXPECT_TRUE(success);
    // Linux does not serialize transfer f_pos with fdget_pos; do not demand
    // atomic transfer-position accounting across this race.
    close(target);
}

TEST_F(SharedFilePosition, ReverseSendfileEndpointsDoNotDeadlock) {
    ASSERT_NO_FATAL_FAILURE(FillNumberedFile());
    char target_path[] = "/tmp/dunitest_position_reverse_XXXXXX";
    int target = mkstemp(target_path);
    ASSERT_GE(target, 0);
    ASSERT_EQ(0, unlink(target_path));
    std::vector<char> initial(kBytes, 'x');
    ASSERT_EQ(static_cast<ssize_t>(initial.size()), write(target, initial.data(), initial.size()));
    ASSERT_EQ(0, lseek(target, 0, SEEK_SET));
    std::array<bool, kThreads> valid;
    valid.fill(true);
    Concurrent([&](size_t thread) {
        for (size_t call = 0; call < 32; ++call) {
            int source = thread % 2 == 0 ? fd : target;
            int destination = thread % 2 == 0 ? target : fd;
            if (syscall(SYS_sendfile, destination, source, nullptr, 64) != 64) {
                valid[thread] = false;
            }
        }
    });
    for (bool success : valid) EXPECT_TRUE(success);
    close(target);
}

TEST_F(SharedFilePosition, FullPipeSendfileDoesNotHoldInputPositionWhileWaiting) {
    ASSERT_NO_FATAL_FAILURE(FillNumberedFile());
    int pipe_fds[2];
    ASSERT_EQ(0, pipe2(pipe_fds, O_NONBLOCK));
    std::array<char, 4096> bytes{};
    while (write(pipe_fds[1], bytes.data(), bytes.size()) > 0) {}
    ASSERT_EQ(EAGAIN, errno);
    ASSERT_EQ(0, fcntl(pipe_fds[1], F_SETFL, 0));
    std::atomic<bool> entering{false};
    ssize_t sent = -1;
    std::thread sender([&] {
        entering.store(true, std::memory_order_release);
        sent = syscall(SYS_sendfile, pipe_fds[1], fd, nullptr, 1);
    });
    while (!entering.load(std::memory_order_acquire)) sched_yield();
    usleep(20000);
    char byte;
    // This read must finish before we make space for the blocked transfer.
    // Holding f_pos across its pipe wait introduces an AB/BA wait cycle.
    EXPECT_EQ(1, read(copies[0], &byte, 1));
    EXPECT_EQ(static_cast<ssize_t>(bytes.size()), read(pipe_fds[0], bytes.data(), bytes.size()));
    sender.join();
    EXPECT_EQ(1, sent);
    EXPECT_EQ(2, lseek(fd, 0, SEEK_CUR));
    close(pipe_fds[0]);
    close(pipe_fds[1]);
}

TEST_F(SharedFilePosition, EmptyPipeSpliceDoesNotHoldOutputPositionWhileWaiting) {
    ASSERT_NO_FATAL_FAILURE(FillNumberedFile());
    int pipe_fds[2];
    ASSERT_EQ(0, pipe(pipe_fds));
    std::atomic<bool> entering{false};
    ssize_t moved = -1;
    std::thread receiver([&] {
        entering.store(true, std::memory_order_release);
        moved = splice(pipe_fds[0], nullptr, fd, nullptr, 1, 0);
    });
    while (!entering.load(std::memory_order_acquire)) sched_yield();
    usleep(20000);
    char byte;
    EXPECT_EQ(1, read(copies[0], &byte, 1));
    EXPECT_EQ(1, write(pipe_fds[1], "s", 1));
    receiver.join();
    EXPECT_EQ(1, moved);
    EXPECT_EQ(2, lseek(fd, 0, SEEK_CUR));
    char stored = 0;
    EXPECT_EQ(1, pread(fd, &stored, 1, 1));
    EXPECT_EQ('s', stored);
    close(pipe_fds[0]);
    close(pipe_fds[1]);
}
TEST_F(SharedFilePosition, SharedDirectoryIterationKeepsOneCoherentCursor) {
    constexpr size_t kEntries = 32;
    struct Directory {
        char path[sizeof("/tmp/dunitest_position_dir_XXXXXX")] =
            "/tmp/dunitest_position_dir_XXXXXX";
        int fd = -1;
        std::array<int, kThreads> aliases{{-1, -1, -1, -1}};
        ~Directory() {
            for (int alias : aliases) if (alias >= 0) close(alias);
            if (fd >= 0) {
                for (size_t i = 0; i < kEntries; ++i) {
                    const std::string name = "entry_" + std::to_string(i);
                    unlinkat(fd, name.c_str(), 0);
                }
                close(fd);
                rmdir(path);
            }
        }
    } directory;
    ASSERT_NE(nullptr, mkdtemp(directory.path));
    directory.fd = open(directory.path, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (directory.fd < 0) {
        rmdir(directory.path);
        FAIL() << strerror(errno);
    }
    for (size_t i = 0; i < kEntries; ++i) {
        const std::string name = "entry_" + std::to_string(i);
        int created = openat(directory.fd, name.c_str(), O_CREAT | O_EXCL | O_WRONLY, 0600);
        ASSERT_GE(created, 0) << strerror(errno);
        ASSERT_EQ(0, close(created));
    }
    for (int& alias : directory.aliases) {
        alias = dup(directory.fd);
        ASSERT_GE(alias, 0);
    }
    std::array<char, 128> first{};
    const ssize_t first_count = syscall(SYS_getdents64, directory.fd, first.data(), first.size());
    ASSERT_GT(first_count, 0);
    ASSERT_LE(first_count, static_cast<ssize_t>(first.size()));
    ASSERT_EQ(0, lseek(directory.fd, 0, SEEK_SET));
    std::array<std::vector<std::string>, kThreads> names;
    std::array<bool, kThreads> valid;
    valid.fill(false);
    Concurrent([&](size_t thread) {
        // Linux dirent64 places d_reclen at byte 16 and d_name at byte 19.
        // memcpy avoids assuming any alignment of individual records.
        constexpr size_t kNameOffset = 19;
        for (size_t call = 0; call < 256; ++call) {
            std::array<char, 128> buffer{};
            const ssize_t count = syscall(SYS_getdents64, directory.aliases[thread],
                                          buffer.data(), buffer.size());
            if (count == 0) {
                valid[thread] = true;
                return;
            }
            if (count < 0 || static_cast<size_t>(count) > buffer.size()) return;
            size_t offset = 0;
            while (offset < static_cast<size_t>(count)) {
                const size_t remaining = static_cast<size_t>(count) - offset;
                if (remaining <= kNameOffset) return;
                uint16_t record_len = 0;
                memcpy(&record_len, buffer.data() + offset + 16, sizeof(record_len));
                if (record_len <= kNameOffset || record_len > remaining) return;
                const char* name = buffer.data() + offset + kNameOffset;
                const char* end = static_cast<const char*>(memchr(name, '\0',
                                                                 record_len - kNameOffset));
                if (end == nullptr) return;
                names[thread].emplace_back(name, end);
                offset += record_len;
            }
        }
    });
    std::vector<std::string> actual;
    for (size_t thread = 0; thread < kThreads; ++thread) {
        EXPECT_TRUE(valid[thread]) << "directory reader " << thread;
        actual.insert(actual.end(), names[thread].begin(), names[thread].end());
    }
    std::vector<std::string> expected{".", ".."};
    for (size_t i = 0; i < kEntries; ++i) expected.push_back("entry_" + std::to_string(i));
    std::sort(actual.begin(), actual.end());
    std::sort(expected.begin(), expected.end());
    EXPECT_EQ(expected, actual);
    std::array<char, 128> repeated{};
    EXPECT_EQ(0, syscall(SYS_getdents64, directory.fd, repeated.data(), repeated.size()));
    ASSERT_EQ(0, lseek(directory.fd, 0, SEEK_SET));
    ASSERT_EQ(first_count,
              syscall(SYS_getdents64, directory.fd, repeated.data(), repeated.size()));
    EXPECT_EQ(0, memcmp(first.data(), repeated.data(), static_cast<size_t>(first_count)));
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
