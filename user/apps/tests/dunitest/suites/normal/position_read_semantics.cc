#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <unistd.h>

#include <array>
#include <vector>

namespace {
enum class Operation { Scalar, Vector, Vector2 };
ssize_t ReadAt(Operation op, int fd, void* output, size_t length, off_t offset) {
    if (op == Operation::Scalar) return pread(fd, output, length, offset);
    iovec vector{output, length};
    if (op == Operation::Vector) return preadv(fd, &vector, 1, offset);
    return syscall(SYS_preadv2, fd, &vector, 1, offset, 0UL, 0U);
}
ssize_t ReadVectors(Operation op, int fd, const iovec* vectors, size_t count,
                    off_t offset) {
    if (op == Operation::Vector) return preadv(fd, vectors, count, offset);
    return syscall(SYS_preadv2, fd, vectors, count, offset, 0UL, 0U);
}
unsigned char Pattern(size_t position) { return (position * 37 + position / 251 + 11) & 255; }
class PositionRead : public testing::TestWithParam<Operation> {
 protected:
    void SetUp() override {
        char path[] = "/tmp/position-read-XXXXXX";
        fd_ = mkstemp(path);
        ASSERT_GE(fd_, 0) << strerror(errno);
        ASSERT_EQ(0, unlink(path));
        std::vector<unsigned char> source(3 * 65536 + 137);
        for (size_t i = 0; i < source.size(); ++i) source[i] = Pattern(i);
        ASSERT_EQ(static_cast<ssize_t>(source.size()), write(fd_, source.data(), source.size()));
        size_ = source.size();
        ASSERT_EQ(13, lseek(fd_, 13, SEEK_SET));
        page_ = sysconf(_SC_PAGESIZE);
        mapping_ = static_cast<unsigned char*>(mmap(nullptr, page_ * 2, PROT_READ | PROT_WRITE,
                                                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
        ASSERT_NE(MAP_FAILED, mapping_);
        ASSERT_EQ(0, mprotect(mapping_ + page_, page_, PROT_NONE));
    }
    void TearDown() override {
        if (mapping_ != MAP_FAILED) munmap(mapping_, page_ * 2);
        if (fd_ >= 0) close(fd_);
    }
    void CheckPattern(const unsigned char* data, size_t count, size_t offset) {
        for (size_t i = 0; i < count; ++i) ASSERT_EQ(Pattern(offset + i), data[i]) << i;
    }
    int fd_ = -1;
    size_t size_ = 0, page_ = 0;
    unsigned char* mapping_ = static_cast<unsigned char*>(MAP_FAILED);
};
TEST_P(PositionRead, EofPrecedesMappedDestinationFault) {
    EXPECT_EQ(0, ReadAt(GetParam(), fd_, mapping_ + page_, 17, size_));
    EXPECT_EQ(13, lseek(fd_, 0, SEEK_CUR));
}
TEST_P(PositionRead, EofPrecedesNullDestinationFault) {
    EXPECT_EQ(0, ReadAt(GetParam(), fd_, nullptr, 17, size_));
    EXPECT_EQ(13, lseek(fd_, 0, SEEK_CUR));
}
TEST_P(PositionRead, ShortFileReadDoesNotTouchFaultingTail) {
    auto* output = mapping_ + page_ - 7;
    ASSERT_EQ(7, ReadAt(GetParam(), fd_, output, 64, size_ - 7));
    CheckPattern(output, 7, size_ - 7);
    EXPECT_EQ(13, lseek(fd_, 0, SEEK_CUR));
}
TEST_P(PositionRead, PartialDestinationFaultReturnsCopiedPrefix) {
    auto* output = mapping_ + page_ - 64;
    ASSERT_EQ(64, ReadAt(GetParam(), fd_, output, 128, 19)) << strerror(errno);
    CheckPattern(output, 64, 19);
    EXPECT_EQ(13, lseek(fd_, 0, SEEK_CUR));
}
TEST_P(PositionRead, InitialFaultPreservesTheOpenFilePosition) {
    errno = 0;
    EXPECT_EQ(-1, ReadAt(GetParam(), fd_, mapping_ + page_, 32, 19));
    EXPECT_EQ(EFAULT, errno);
    EXPECT_EQ(13, lseek(fd_, 0, SEEK_CUR));
}
TEST_P(PositionRead, ChunkBoundariesPreserveDataAndOpenFilePosition) {
    for (size_t length : {size_t(2049), size_t(4097), size_t(65536), size_t(131163)}) {
        std::vector<unsigned char> output(length);
        ASSERT_EQ(static_cast<ssize_t>(length), ReadAt(GetParam(), fd_, output.data(), length, 29));
        CheckPattern(output.data(), length, 29);
        EXPECT_EQ(13, lseek(fd_, 0, SEEK_CUR));
    }
}
TEST_P(PositionRead, EmptyRequestDoesNotTouchDestination) {
    EXPECT_EQ(0, ReadAt(GetParam(), fd_, mapping_ + page_, 0, 19));
    EXPECT_EQ(13, lseek(fd_, 0, SEEK_CUR));
}
TEST_P(PositionRead, ReadModePrecedesMappedDestinationFault) {
    char path[64];
    snprintf(path, sizeof(path), "/proc/self/fd/%d", fd_);
    int write_only = open(path, O_WRONLY | O_CLOEXEC);
    ASSERT_GE(write_only, 0);
    errno = 0;
    EXPECT_EQ(-1, ReadAt(GetParam(), write_only, mapping_ + page_, 1, 0));
    EXPECT_EQ(EBADF, errno);
    close(write_only);
}
TEST_P(PositionRead, NonseekableCapabilityPrecedesDestinationFault) {
    int descriptors[2];
    ASSERT_EQ(0, pipe2(descriptors, O_NONBLOCK | O_CLOEXEC));
    errno = 0;
    EXPECT_EQ(-1, ReadAt(GetParam(), descriptors[0], mapping_ + page_, 1, 0));
    EXPECT_EQ(ESPIPE, errno);
    close(descriptors[0]); close(descriptors[1]);
}
TEST_P(PositionRead, ProcReadKeepsItsExplicitOffsetAndEofPriority) {
    int proc = open("/proc/self/oom_score_adj", O_RDONLY | O_CLOEXEC);
    ASSERT_GE(proc, 0);
    char expected[32] = {};
    ssize_t count = pread(proc, expected, sizeof(expected), 0);
    ASSERT_GT(count, 1);
    ASSERT_EQ(3, lseek(proc, 3, SEEK_SET));
    char output[32] = {};
    ASSERT_EQ(count - 1, ReadAt(GetParam(), proc, output, sizeof(output), 1));
    EXPECT_EQ(0, memcmp(output, expected + 1, count - 1));
    EXPECT_EQ(0, ReadAt(GetParam(), proc, mapping_ + page_, 32, count));
    EXPECT_EQ(3, lseek(proc, 0, SEEK_CUR));
    close(proc);
}
INSTANTIATE_TEST_SUITE_P(Operations, PositionRead,
                        testing::Values(Operation::Scalar, Operation::Vector, Operation::Vector2));

class PositionReadVectors : public PositionRead {};
TEST_P(PositionReadVectors, PartialLaterIovecReturnsTheCompleteSuccessfulPrefix) {
    std::array<unsigned char, 17> first{};
    auto* tail = mapping_ + page_ - 64;
    iovec vectors[] = {{first.data(), first.size()}, {tail, 128}};
    ASSERT_EQ(81, ReadVectors(GetParam(), fd_, vectors, 2, 19));
    CheckPattern(first.data(), first.size(), 19);
    CheckPattern(tail, 64, 36);
    EXPECT_EQ(13, lseek(fd_, 0, SEEK_CUR));
}
TEST_P(PositionReadVectors, OverlappingIovecsAreWrittenInOrder) {
    std::array<unsigned char, 16> output{};
    iovec vectors[] = {{output.data(), 16}, {output.data() + 4, 8}};
    ASSERT_EQ(24, ReadVectors(GetParam(), fd_, vectors, 2, 19));
    CheckPattern(output.data(), 4, 19);
    CheckPattern(output.data() + 4, 8, 35);
    CheckPattern(output.data() + 12, 4, 31);
    EXPECT_EQ(13, lseek(fd_, 0, SEEK_CUR));
}
TEST_P(PositionReadVectors, MaximumIovecsAndZeroEntriesRetainTheSnapshot) {
    std::array<unsigned char, 1024> output{};
    std::array<iovec, 1024> vectors{};
    size_t nonempty = 0;
    for (size_t i = 0; i < vectors.size(); ++i) {
        const bool empty = i % 3 == 0;
        vectors[i] = {empty ? mapping_ + page_ : output.data() + nonempty, empty ? 0U : 1U};
        if (!empty) ++nonempty;
    }
    ASSERT_EQ(static_cast<ssize_t>(nonempty), ReadVectors(GetParam(), fd_, vectors.data(), vectors.size(), 29));
    CheckPattern(output.data(), nonempty, 29);
    EXPECT_EQ(13, lseek(fd_, 0, SEEK_CUR));
}
TEST_P(PositionReadVectors, DescriptorLookupPrecedesMetadataFault) {
    errno = 0;
    EXPECT_EQ(-1, ReadVectors(GetParam(), -1, reinterpret_cast<iovec*>(mapping_ + page_), 1, 0));
    EXPECT_EQ(EBADF, errno);
}
TEST_P(PositionReadVectors, LegacyProcReaderAdvancesPastASuccessfulEmptyIovec) {
    int proc = open("/proc/self/oom_score_adj", O_RDONLY | O_CLOEXEC);
    ASSERT_GE(proc, 0);
    char output[32] = {};
    iovec vectors[] = {{mapping_ + page_, 0}, {output, sizeof(output)}};
    char expected[32] = {};
    const ssize_t length = pread(proc, expected, sizeof(expected), 0);
    ASSERT_GT(length, 0);
    EXPECT_EQ(length, ReadVectors(GetParam(), proc, vectors, 2, 0));
    EXPECT_EQ(0, memcmp(output, expected, length));
    close(proc);
}
INSTANTIATE_TEST_SUITE_P(Operations, PositionReadVectors,
                        testing::Values(Operation::Vector, Operation::Vector2));
TEST(PositionReadOwner, ProcMemUsesTheExplicitSourceAddress) {
    int proc = open("/proc/self/mem", O_RDONLY | O_CLOEXEC);
    ASSERT_GE(proc, 0);
    const char source[] = "addressed-memory";
    char output[sizeof(source)] = {};
    ASSERT_EQ(3, lseek(proc, 3, SEEK_SET));
    for (Operation op : {Operation::Scalar, Operation::Vector, Operation::Vector2}) {
        memset(output, 0, sizeof(output));
        ASSERT_EQ(static_cast<ssize_t>(sizeof(source)),
                  ReadAt(op, proc, output, sizeof(output), reinterpret_cast<uintptr_t>(source)));
        EXPECT_EQ(0, memcmp(source, output, sizeof(source)));
        EXPECT_EQ(3, lseek(proc, 0, SEEK_CUR));
    }
    close(proc);
}
}  // namespace
int main(int argc, char** argv) { testing::InitGoogleTest(&argc, argv); return RUN_ALL_TESTS(); }
