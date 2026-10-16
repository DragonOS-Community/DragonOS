#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <vector>

namespace {
constexpr size_t kChunk = 64 * 1024;
constexpr unsigned char kCanary = 0xcd;

unsigned char Pattern(size_t offset) {
    return static_cast<unsigned char>((offset * 37 + offset / 251 + 11) & 0xff);
}

void ExpectPattern(const unsigned char* bytes, size_t length, size_t offset = 0) {
    for (size_t i = 0; i < length; ++i) {
        ASSERT_EQ(Pattern(offset + i), bytes[i]) << "byte " << i << ", offset " << offset;
    }
}

void ExpectCanary(const unsigned char* bytes, size_t length) {
    for (size_t i = 0; i < length; ++i) ASSERT_EQ(kCanary, bytes[i]) << "byte " << i;
}

class Mapping {
  public:
    explicit Mapping(size_t length) : length_(length) {
        bytes = static_cast<unsigned char*>(mmap(nullptr, length, PROT_READ | PROT_WRITE,
                                                MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    }
    ~Mapping() {
        if (bytes != MAP_FAILED) munmap(bytes, length_);
    }
    Mapping(const Mapping&) = delete;
    Mapping& operator=(const Mapping&) = delete;
    unsigned char* bytes;

  private:
    size_t length_;
};

class ReadBufferSemantics : public ::testing::Test {
  protected:
    void SetUp() override {
        char path[] = "/tmp/dunitest_read_buffer_XXXXXX";
        fd = mkstemp(path);
        ASSERT_GE(fd, 0) << strerror(errno);
        ASSERT_EQ(0, unlink(path));
        std::vector<unsigned char> source(kChunk * 3 + 137);
        for (size_t i = 0; i < source.size(); ++i) source[i] = Pattern(i);
        size_t written = 0;
        while (written < source.size()) {
            ssize_t count = write(fd, source.data() + written, source.size() - written);
            if (count < 0 && errno == EINTR) continue;
            ASSERT_GT(count, 0) << strerror(errno);
            written += static_cast<size_t>(count);
        }
        file_size = source.size();
        ASSERT_EQ(0, lseek(fd, 0, SEEK_SET));
    }
    void TearDown() override {
        if (fd >= 0) close(fd);
    }
    int fd = -1;
    size_t file_size = 0;
};

TEST_F(ReadBufferSemantics, ScalarChunkBoundariesPreserveDataAndOffset) {
    for (size_t length : {size_t(1), size_t(2048), size_t(2049), size_t(4096), size_t(4097),
                          kChunk, kChunk + 1, kChunk * 2 + 91}) {
        SCOPED_TRACE(length);
        ASSERT_EQ(0, lseek(fd, 0, SEEK_SET));
        std::vector<unsigned char> output(length + 17, kCanary);
        ASSERT_EQ(static_cast<ssize_t>(length), read(fd, output.data(), length)) << strerror(errno);
        ASSERT_NO_FATAL_FAILURE(ExpectPattern(output.data(), length));
        ASSERT_NO_FATAL_FAILURE(ExpectCanary(output.data() + length, 17));
        EXPECT_EQ(static_cast<off_t>(length), lseek(fd, 0, SEEK_CUR));
    }
}

TEST_F(ReadBufferSemantics, VectoredGrowthAndCurrentOffsetPreserveData) {
    std::array<unsigned char, 33> first;
    first.fill(kCanary);
    std::vector<unsigned char> second(kChunk + 17 + 19, kCanary);
    std::vector<unsigned char> third(4097 + 19, kCanary);
    iovec iovs[] = {{nullptr, 0}, {first.data(), 17}, {second.data(), kChunk + 17},
                   {nullptr, 0}, {third.data(), 4097}};
    constexpr size_t total = 17 + kChunk + 17 + 4097;
    ASSERT_EQ(static_cast<ssize_t>(total), readv(fd, iovs, 5)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(ExpectPattern(first.data(), 17));
    ASSERT_NO_FATAL_FAILURE(ExpectCanary(first.data() + 17, 16));
    ASSERT_NO_FATAL_FAILURE(ExpectPattern(second.data(), kChunk + 17, 17));
    ASSERT_NO_FATAL_FAILURE(ExpectCanary(second.data() + kChunk + 17, 19));
    ASSERT_NO_FATAL_FAILURE(ExpectPattern(third.data(), 4097, 17 + kChunk + 17));
    ASSERT_NO_FATAL_FAILURE(ExpectCanary(third.data() + 4097, 19));
    EXPECT_EQ(static_cast<off_t>(total), lseek(fd, 0, SEEK_CUR));

    // Linux preadv2(-1) uses and advances the shared file position.
    std::array<unsigned char, 71> next = {};
    iovec current[] = {{next.data(), 23}, {next.data() + 23, 48}};
    ASSERT_EQ(static_cast<ssize_t>(next.size()),
              syscall(SYS_preadv2, fd, current, 2, -1L, -1L, 0L)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(ExpectPattern(next.data(), next.size(), total));
    EXPECT_EQ(static_cast<off_t>(total + next.size()), lseek(fd, 0, SEEK_CUR));
}

TEST_F(ReadBufferSemantics, OverlappingIovecsAreProcessedInOrder) {
    std::array<unsigned char, 64> output;
    output.fill(kCanary);
    iovec iovs[] = {{output.data(), 32}, {output.data() + 16, 32}};
    ASSERT_EQ(64, readv(fd, iovs, 2));
    ASSERT_NO_FATAL_FAILURE(ExpectPattern(output.data(), 16));
    ASSERT_NO_FATAL_FAILURE(ExpectPattern(output.data() + 16, 32, 32));
    ASSERT_NO_FATAL_FAILURE(ExpectCanary(output.data() + 48, 16));
    EXPECT_EQ(64, lseek(fd, 0, SEEK_CUR));
}

TEST_F(ReadBufferSemantics, ShortReadStopsBeforeFaultingLaterIovec) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    Mapping mapping(page * 2);
    ASSERT_NE(MAP_FAILED, mapping.bytes);
    memset(mapping.bytes, kCanary, page);
    ASSERT_EQ(0, mprotect(mapping.bytes + page, page, PROT_NONE));
    constexpr size_t remaining = 123;
    const size_t start = file_size - remaining;
    ASSERT_EQ(static_cast<off_t>(start), lseek(fd, start, SEEK_SET));
    iovec iovs[] = {{mapping.bytes, page}, {mapping.bytes + page, page}};
    ASSERT_EQ(static_cast<ssize_t>(remaining), readv(fd, iovs, 2));
    ASSERT_NO_FATAL_FAILURE(ExpectPattern(mapping.bytes, remaining, start));
    ASSERT_NO_FATAL_FAILURE(ExpectCanary(mapping.bytes + remaining, page - remaining));
    EXPECT_EQ(static_cast<off_t>(file_size), lseek(fd, 0, SEEK_CUR));
    EXPECT_EQ(0, read(fd, mapping.bytes, page));
    EXPECT_EQ(0, readv(fd, iovs, 2));

    ASSERT_EQ(static_cast<off_t>(start), lseek(fd, start, SEEK_SET));
    memset(mapping.bytes, kCanary, page);
    ASSERT_EQ(static_cast<ssize_t>(remaining), read(fd, mapping.bytes, page * 2));
    ASSERT_NO_FATAL_FAILURE(ExpectPattern(mapping.bytes, remaining, start));
    ASSERT_NO_FATAL_FAILURE(ExpectCanary(mapping.bytes + remaining, page - remaining));
}

TEST_F(ReadBufferSemantics, PartialFaultReturnsCommittedPrefixAndAdvancesOffset) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    const size_t accessible = kChunk + page;
    Mapping mapping(accessible + page);
    ASSERT_NE(MAP_FAILED, mapping.bytes);
    ASSERT_EQ(0, mprotect(mapping.bytes + accessible, page, PROT_NONE));
    ASSERT_EQ(static_cast<ssize_t>(accessible), read(fd, mapping.bytes, accessible + page));
    ASSERT_NO_FATAL_FAILURE(ExpectPattern(mapping.bytes, accessible));
    EXPECT_EQ(static_cast<off_t>(accessible), lseek(fd, 0, SEEK_CUR));

    ASSERT_EQ(0, lseek(fd, 0, SEEK_SET));
    std::array<unsigned char, 17> first = {};
    iovec iovs[] = {{first.data(), first.size()}, {mapping.bytes, accessible + page}};
    ASSERT_EQ(static_cast<ssize_t>(first.size() + accessible), readv(fd, iovs, 2));
    ASSERT_NO_FATAL_FAILURE(ExpectPattern(first.data(), first.size()));
    ASSERT_NO_FATAL_FAILURE(ExpectPattern(mapping.bytes, accessible, first.size()));
    EXPECT_EQ(static_cast<off_t>(first.size() + accessible), lseek(fd, 0, SEEK_CUR));
}

TEST_F(ReadBufferSemantics, InitialFaultDoesNotConsumeFileData) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    Mapping mapping(page);
    ASSERT_NE(MAP_FAILED, mapping.bytes);
    ASSERT_EQ(0, mprotect(mapping.bytes, page, PROT_NONE));
    errno = 0;
    EXPECT_EQ(-1, read(fd, mapping.bytes, page));
    EXPECT_EQ(EFAULT, errno);
    EXPECT_EQ(0, lseek(fd, 0, SEEK_CUR));
    iovec invalid = {mapping.bytes, page};
    errno = 0;
    EXPECT_EQ(-1, readv(fd, &invalid, 1));
    EXPECT_EQ(EFAULT, errno);
    EXPECT_EQ(0, lseek(fd, 0, SEEK_CUR));
    ASSERT_EQ(0, mprotect(mapping.bytes, page, PROT_READ | PROT_WRITE));
    ASSERT_EQ(static_cast<ssize_t>(page), read(fd, mapping.bytes, page));
    ASSERT_NO_FATAL_FAILURE(ExpectPattern(mapping.bytes, page));
}

TEST_F(ReadBufferSemantics, EmptyRequestsStillValidateDescriptor) {
    unsigned char output = kCanary;
    iovec empty = {nullptr, 0};
    EXPECT_EQ(0, read(fd, nullptr, 0));
    EXPECT_EQ(0, readv(fd, nullptr, 0));
    EXPECT_EQ(0, readv(fd, &empty, 1));
    EXPECT_EQ(0, lseek(fd, 0, SEEK_CUR));
    EXPECT_EQ(kCanary, output);
    errno = 0;
    EXPECT_EQ(-1, read(-1, &output, 0));
    EXPECT_EQ(EBADF, errno);
    errno = 0;
    EXPECT_EQ(-1, readv(-1, nullptr, 0));
    EXPECT_EQ(EBADF, errno);

    int path_fd = open("/tmp", O_PATH | O_CLOEXEC);
    ASSERT_GE(path_fd, 0) << strerror(errno);
    errno = 0;
    EXPECT_EQ(-1, read(path_fd, &output, 0));
    EXPECT_EQ(EBADF, errno);
    errno = 0;
    EXPECT_EQ(-1, readv(path_fd, nullptr, 0));
    EXPECT_EQ(EBADF, errno);
    iovec positive = {&output, 1};
    errno = 0;
    EXPECT_EQ(-1, read(path_fd, &output, 1));
    EXPECT_EQ(EBADF, errno);
    errno = 0;
    EXPECT_EQ(-1, readv(path_fd, &positive, 1));
    EXPECT_EQ(EBADF, errno);
    close(path_fd);

    int write_only = open("/dev/null", O_WRONLY | O_CLOEXEC);
    ASSERT_GE(write_only, 0) << strerror(errno);
    for (size_t length : {size_t(0), size_t(1)}) {
        SCOPED_TRACE(length);
        positive.iov_len = length;
        errno = 0;
        EXPECT_EQ(-1, read(write_only, &output, length));
        EXPECT_EQ(EBADF, errno);
        errno = 0;
        EXPECT_EQ(-1, readv(write_only, &positive, 1));
        EXPECT_EQ(EBADF, errno);
    }
    close(write_only);
}

TEST_F(ReadBufferSemantics, CpuOnlyDevicesPreserveZeroAndEofResults) {
    const size_t size = kChunk * 2 + 73;
    std::vector<unsigned char> output(size + 11, kCanary);
    int zero = open("/dev/zero", O_RDONLY | O_CLOEXEC);
    ASSERT_GE(zero, 0) << strerror(errno);
    ASSERT_EQ(static_cast<ssize_t>(size), read(zero, output.data(), size));
    EXPECT_TRUE(std::all_of(output.begin(), output.begin() + size,
                            [](unsigned char value) { return value == 0; }));
    ASSERT_NO_FATAL_FAILURE(ExpectCanary(output.data() + size, 11));
    std::fill(output.begin(), output.end(), kCanary);
    iovec iovs[] = {{output.data(), 17}, {output.data() + 17, size - 17}};
    ASSERT_EQ(static_cast<ssize_t>(size), readv(zero, iovs, 2));
    EXPECT_TRUE(std::all_of(output.begin(), output.begin() + size,
                            [](unsigned char value) { return value == 0; }));
    ASSERT_NO_FATAL_FAILURE(ExpectCanary(output.data() + size, 11));
    close(zero);

    int null_fd = open("/dev/null", O_RDONLY | O_CLOEXEC);
    ASSERT_GE(null_fd, 0) << strerror(errno);
    std::fill(output.begin(), output.end(), kCanary);
    EXPECT_EQ(0, read(null_fd, output.data(), size));
    EXPECT_EQ(0, readv(null_fd, iovs, 2));
    ASSERT_NO_FATAL_FAILURE(ExpectCanary(output.data(), output.size()));
    close(null_fd);
}
}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
