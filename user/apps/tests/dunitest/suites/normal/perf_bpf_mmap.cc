// perf BPF-output mmap backing lifecycle and publication regression tests.

#include <gtest/gtest.h>

#include <errno.h>
#include <linux/perf_event.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include <cstdint>
#include <cstring>
#include <string>

namespace {

class FdGuard {
  public:
    explicit FdGuard(int fd = -1) : fd_(fd) {}
    ~FdGuard() {
        if (fd_ >= 0) close(fd_);
    }
    int Get() const { return fd_; }

  private:
    int fd_;
};

std::string Error() {
    return std::to_string(errno) + " (" + std::strerror(errno) + ")";
}

int OpenBpfOutputEvent() {
    struct perf_event_attr attr {};
    attr.type = PERF_TYPE_SOFTWARE;
    attr.size = sizeof(attr);
    attr.config = PERF_COUNT_SW_BPF_OUTPUT;
    attr.sample_type = PERF_SAMPLE_RAW;
    return static_cast<int>(
        syscall(SYS_perf_event_open, &attr, -1, 0, -1, 0));
}

void* Map(int fd, size_t length) {
    return mmap(nullptr, length, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
}

TEST(PerfBpfMmap, GeometrySharingAndFailedRequestIsolation) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    FdGuard fd(OpenBpfOutputEvent());
    ASSERT_GE(fd.Get(), 0) << Error();
    struct stat before {};
    ASSERT_EQ(fstat(fd.Get(), &before), 0) << Error();
    EXPECT_EQ(before.st_size, 0);

    // Four total pages means three data pages, which is not a power of two.
    errno = 0;
    EXPECT_EQ(Map(fd.Get(), page * 4), MAP_FAILED);
    EXPECT_EQ(errno, EINVAL) << Error();

    const size_t length = page * 3;  // metadata + two data pages
    auto* first = static_cast<uint8_t*>(Map(fd.Get(), length));
    ASSERT_NE(first, MAP_FAILED) << Error();
    auto* metadata = reinterpret_cast<perf_event_mmap_page*>(first);
    EXPECT_EQ(metadata->data_offset, page);
    EXPECT_EQ(metadata->data_size, page * 2);
    struct stat after {};
    ASSERT_EQ(fstat(fd.Get(), &after), 0) << Error();
    EXPECT_EQ(after.st_size, 0);

    auto* second = static_cast<uint8_t*>(Map(fd.Get(), length));
    ASSERT_NE(second, MAP_FAILED) << Error();
    first[page + 128] = 0x5a;
    EXPECT_EQ(second[page + 128], 0x5a);

    // A valid but different geometry must not replace the live backing.
    errno = 0;
    EXPECT_EQ(Map(fd.Get(), page * 5), MAP_FAILED);
    EXPECT_EQ(errno, EINVAL) << Error();
    EXPECT_EQ(metadata->data_offset, page);
    EXPECT_EQ(second[page + 128], 0x5a);

    EXPECT_EQ(munmap(second, length), 0) << Error();
    EXPECT_EQ(munmap(first, length), 0) << Error();
}

TEST(PerfBpfMmap, ConcurrentSameGeometryPublishesOneBacking) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    const size_t length = page * 3;
    FdGuard fd(OpenBpfOutputEvent());
    ASSERT_GE(fd.Get(), 0) << Error();

    int start_pipe[2];
    int ready_pipe[2];
    ASSERT_EQ(pipe(start_pipe), 0) << Error();
    ASSERT_EQ(pipe(ready_pipe), 0) << Error();
    const pid_t child = fork();
    ASSERT_GE(child, 0) << Error();
    if (child == 0) {
        close(start_pipe[1]);
        close(ready_pipe[0]);
        char byte = 0;
        if (read(start_pipe[0], &byte, 1) != 1) _exit(2);
        void* mapping = Map(fd.Get(), length);
        if (mapping == MAP_FAILED) _exit(3);
        if (write(ready_pipe[1], "r", 1) != 1) _exit(4);
        if (read(start_pipe[0], &byte, 1) != 1) _exit(5);
        const auto* bytes = static_cast<volatile uint8_t*>(mapping);
        if (bytes[page + 256] != 0xa5) _exit(6);
        if (munmap(mapping, length) != 0) _exit(7);
        _exit(0);
    }

    close(start_pipe[0]);
    close(ready_pipe[1]);
    ASSERT_EQ(write(start_pipe[1], "s", 1), 1);
    void* parent_mapping = Map(fd.Get(), length);
    ASSERT_NE(parent_mapping, MAP_FAILED) << Error();
    char byte = 0;
    ASSERT_EQ(read(ready_pipe[0], &byte, 1), 1);
    static_cast<volatile uint8_t*>(parent_mapping)[page + 256] = 0xa5;
    ASSERT_EQ(write(start_pipe[1], "v", 1), 1);
    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(WEXITSTATUS(status), 0);
    EXPECT_EQ(munmap(parent_mapping, length), 0) << Error();
    close(start_pipe[1]);
    close(ready_pipe[0]);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
