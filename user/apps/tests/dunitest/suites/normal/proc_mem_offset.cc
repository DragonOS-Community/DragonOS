// /proc/pid/mem offsets are address bit patterns, not signed file lengths.
// Keep the libc error-range ambiguity separate from the actual f_pos change.
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <sys/uio.h>
#include <unistd.h>

#include <array>
#include <cstdlib>
#include <cstring>
#include <limits>

#include <gtest/gtest.h>

namespace {

class ProcMemOffset : public ::testing::Test {
protected:
    void SetUp() override {
        fd_ = open("/proc/self/mem", O_RDWR | O_CLOEXEC);
        ASSERT_GE(fd_, 0) << strerror(errno);
    }

    void TearDown() override {
        if (fd_ >= 0) {
            close(fd_);
        }
    }

    void Seek(off_t offset) {
        errno = 0;
        ASSERT_EQ(offset, lseek(fd_, offset, SEEK_SET));
        ASSERT_EQ(0, errno);
    }

    void ExpectPosition(off_t offset) {
        errno = 0;
        EXPECT_EQ(offset, lseek(fd_, 0, SEEK_CUR));
        EXPECT_EQ(0, errno);
    }

    void ExpectIoError(off_t offset, size_t count, int error) {
        Seek(offset);
        std::array<char, 2> bytes{};
        errno = 0;
        EXPECT_EQ(-1, read(fd_, bytes.data(), count));
        EXPECT_EQ(error, errno);
        ExpectPosition(offset);
        errno = 0;
        EXPECT_EQ(-1, write(fd_, bytes.data(), count));
        EXPECT_EQ(error, errno);
        ExpectPosition(offset);
        struct iovec iov = {bytes.data(), count};
        errno = 0;
        EXPECT_EQ(-1, readv(fd_, &iov, 1));
        EXPECT_EQ(error, errno);
        ExpectPosition(offset);
        errno = 0;
        EXPECT_EQ(-1, writev(fd_, &iov, 1));
        EXPECT_EQ(error, errno);
        ExpectPosition(offset);
    }

    int fd_ = -1;
};

TEST_F(ProcMemOffset, NegativeBitPatternsRemainPositions) {
    for (off_t offset : {off_t(-4096), off_t(-8192), std::numeric_limits<off_t>::min()}) {
        SCOPED_TRACE(offset);
        Seek(offset);
        ExpectPosition(offset);
        // Repeated unsigned positions remain valid; adding INT64_MIN to itself
        // wraps through zero instead of failing a signed-overflow check.
        off_t expected = offset == std::numeric_limits<off_t>::min() ? 0 : offset * 2;
        EXPECT_EQ(expected, lseek(fd_, offset, SEEK_CUR));
        ExpectPosition(expected);
    }
}

TEST_F(ProcMemOffset, RelativeSeekWrapsSignedAndUnsignedBoundaries) {
    Seek(std::numeric_limits<off_t>::max());
    EXPECT_EQ(std::numeric_limits<off_t>::min(), lseek(fd_, 1, SEEK_CUR));
    ExpectPosition(std::numeric_limits<off_t>::min());
    EXPECT_EQ(std::numeric_limits<off_t>::max(), lseek(fd_, -1, SEEK_CUR));
    Seek(-4096);
    EXPECT_EQ(0, lseek(fd_, 4096, SEEK_CUR));
    EXPECT_EQ(-4096, lseek(fd_, -4096, SEEK_CUR));
}

TEST_F(ProcMemOffset, LibcErrorRangeDoesNotUndoSuccessfulSeek) {
    // x86_64/riscv64 have no separate syscall error flag. Linux stores f_pos
    // even when libc interprets the returned bits as a negative errno.
    for (int magnitude : {1, 5, 4095}) {
        SCOPED_TRACE(magnitude);
        errno = 0;
        EXPECT_EQ(-1, lseek(fd_, -magnitude, SEEK_SET));
        EXPECT_EQ(magnitude, errno);
        errno = 0;
        EXPECT_EQ(-1, lseek(fd_, 0, SEEK_CUR));
        EXPECT_EQ(magnitude, errno);
        errno = 0;
        EXPECT_EQ(4096 - magnitude, lseek(fd_, 4096, SEEK_CUR));
        EXPECT_EQ(0, errno);
        ExpectPosition(4096 - magnitude);
    }
}

TEST_F(ProcMemOffset, UnsupportedWhencePreservesPosition) {
    Seek(-8192);
    for (int whence : {SEEK_END, SEEK_DATA, SEEK_HOLE, -1, 6}) {
        SCOPED_TRACE(whence);
        errno = 0;
        EXPECT_EQ(-1, lseek(fd_, 123, whence));
        EXPECT_EQ(EINVAL, errno);
        ExpectPosition(-8192);
    }
}

TEST_F(ProcMemOffset, InaccessibleUnsignedAddressesReachMemoryHandler) {
    ExpectIoError(-8192, 1, EIO);
    ExpectIoError(std::numeric_limits<off_t>::min(), 1, EIO);
    ExpectIoError(std::numeric_limits<off_t>::max(), 2, EIO);
}

TEST_F(ProcMemOffset, UnsignedZeroBoundaryIsOverflow) {
    // Use the libc-ambiguous position -2, then verify its side effect by a
    // relative seek after each operation rather than treating errno as f_pos.
    for (size_t count : {size_t(1), size_t(2)}) {
        char bytes[2]{};
        struct iovec iov = {bytes, count};
        for (int operation = 0; operation < 4; ++operation) {
            SCOPED_TRACE(count);
            SCOPED_TRACE(operation);
            errno = 0;
            EXPECT_EQ(-1, lseek(fd_, -2, SEEK_SET));
            EXPECT_EQ(2, errno);
            errno = 0;
            ssize_t result;
            switch (operation) {
                case 0: result = read(fd_, bytes, count); break;
                case 1: result = write(fd_, bytes, count); break;
                case 2: result = readv(fd_, &iov, 1); break;
                default: result = writev(fd_, &iov, 1); break;
            }
            EXPECT_EQ(-1, result);
            EXPECT_EQ(count == 1 ? EIO : EOVERFLOW, errno);
            EXPECT_EQ(0, lseek(fd_, 2, SEEK_CUR));
        }
    }
}

TEST_F(ProcMemOffset, VectorTotalCountChecksUnsignedBoundary) {
    char bytes[2]{};
    struct iovec vectors[] = {{bytes, 1}, {bytes + 1, 1}};
    for (bool write_operation : {false, true}) {
        errno = 0;
        EXPECT_EQ(-1, lseek(fd_, -2, SEEK_SET));
        EXPECT_EQ(2, errno);
        errno = 0;
        EXPECT_EQ(-1, write_operation ? writev(fd_, vectors, 2) : readv(fd_, vectors, 2));
        EXPECT_EQ(EOVERFLOW, errno);
        EXPECT_EQ(0, lseek(fd_, 2, SEEK_CUR));
    }
}

TEST_F(ProcMemOffset, VectoredV2CurrentPositionUsesUnsignedRules) {
    char byte = 0;
    struct iovec iov = {&byte, 1};
    for (bool write_operation : {false, true}) {
        for (off_t position : {off_t(-1), off_t(-8192)}) {
            // Position -1 is ambiguous to libc but must still be stored.
            errno = 0;
            off_t seek_result = lseek(fd_, position, SEEK_SET);
            EXPECT_EQ(position, seek_result);
            EXPECT_EQ(position == -1 ? 1 : 0, errno);
            errno = 0;
            EXPECT_EQ(-1, write_operation ? pwritev2(fd_, &iov, 1, -1, 0)
                                          : preadv2(fd_, &iov, 1, -1, 0));
            EXPECT_EQ(position == -1 ? EOVERFLOW : EIO, errno);
            EXPECT_EQ(0, lseek(fd_, -position, SEEK_CUR));
        }
    }
}

TEST_F(ProcMemOffset, ZeroLengthIoAtUnsignedPosition) {
    errno = 0;
    EXPECT_EQ(-1, lseek(fd_, -1, SEEK_SET));
    EXPECT_EQ(1, errno);
    EXPECT_EQ(0, read(fd_, nullptr, 0));
    EXPECT_EQ(0, write(fd_, nullptr, 0));
    struct iovec iov = {nullptr, 0};
    EXPECT_EQ(0, readv(fd_, &iov, 1));
    EXPECT_EQ(0, writev(fd_, &iov, 1));
    EXPECT_EQ(0, readv(fd_, nullptr, 0));
    EXPECT_EQ(0, writev(fd_, nullptr, 0));
    EXPECT_EQ(0, lseek(fd_, 1, SEEK_CUR));
}

TEST_F(ProcMemOffset, ExplicitNegativePositionsStayInvalid) {
    char byte = 0;
    struct iovec iov = {&byte, 1};
    Seek(123);
    for (off_t offset : {off_t(-1), off_t(-4096), std::numeric_limits<off_t>::min()}) {
        SCOPED_TRACE(offset);
        errno = 0;
        EXPECT_EQ(-1, pread(fd_, &byte, 1, offset));
        EXPECT_EQ(EINVAL, errno);
        errno = 0;
        EXPECT_EQ(-1, pwrite(fd_, &byte, 1, offset));
        EXPECT_EQ(EINVAL, errno);
        errno = 0;
        EXPECT_EQ(-1, preadv(fd_, &iov, 1, offset));
        EXPECT_EQ(EINVAL, errno);
        errno = 0;
        EXPECT_EQ(-1, pwritev(fd_, &iov, 1, offset));
        EXPECT_EQ(EINVAL, errno);
        ExpectPosition(123);
    }
}

TEST_F(ProcMemOffset, ExplicitPositivePositionsCanCrossSignedBoundary) {
    char bytes[2]{};
    struct iovec iov = {bytes, sizeof(bytes)};
    const off_t offset = std::numeric_limits<off_t>::max();
    Seek(123);
    errno = 0;
    EXPECT_EQ(-1, pread(fd_, bytes, sizeof(bytes), offset));
    EXPECT_EQ(EIO, errno);
    errno = 0;
    EXPECT_EQ(-1, pwrite(fd_, bytes, sizeof(bytes), offset));
    EXPECT_EQ(EIO, errno);
    errno = 0;
    EXPECT_EQ(-1, preadv(fd_, &iov, 1, offset));
    EXPECT_EQ(EIO, errno);
    errno = 0;
    EXPECT_EQ(-1, pwritev(fd_, &iov, 1, offset));
    EXPECT_EQ(EIO, errno);
    ExpectPosition(123);
}

TEST_F(ProcMemOffset, MappedMemoryAndDupShareOrdinaryPosition) {
    char memory[] = "abcdef";
    char output[3]{};
    const off_t address = reinterpret_cast<off_t>(memory);
    Seek(address);
    int duplicate = dup(fd_);
    ASSERT_GE(duplicate, 0);
    EXPECT_EQ(2, read(duplicate, output, 2));
    EXPECT_EQ('a', output[0]);
    EXPECT_EQ('b', output[1]);
    ExpectPosition(address + 2);
    EXPECT_EQ(2, write(fd_, "XY", 2));
    EXPECT_EQ('X', memory[2]);
    EXPECT_EQ('Y', memory[3]);
    EXPECT_EQ(address + 4, lseek(duplicate, 0, SEEK_CUR));
    EXPECT_EQ(1, pread(fd_, output, 1, address));
    EXPECT_EQ('a', output[0]);
    EXPECT_EQ(1, pwrite(fd_, "Z", 1, address));
    EXPECT_EQ('Z', memory[0]);
    ExpectPosition(address + 4);
    close(duplicate);
}

TEST_F(ProcMemOffset, OrdinaryFilesKeepSignedPositions) {
    char path[] = "/tmp/dunitest_proc_mem_offset_XXXXXX";
    int regular = mkstemp(path);
    ASSERT_GE(regular, 0);
    unlink(path);
    ASSERT_EQ(7, lseek(regular, 7, SEEK_SET));
    errno = 0;
    EXPECT_EQ(-1, lseek(regular, -4096, SEEK_SET));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(7, lseek(regular, 0, SEEK_CUR));
    char bytes[2]{};
    errno = 0;
    EXPECT_EQ(-1, pread(regular, bytes, sizeof(bytes), std::numeric_limits<off_t>::max()));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, pwrite(regular, bytes, sizeof(bytes), std::numeric_limits<off_t>::max()));
    EXPECT_EQ(EINVAL, errno);
    close(regular);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
