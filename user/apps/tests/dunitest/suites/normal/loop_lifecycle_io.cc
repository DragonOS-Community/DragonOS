#include <gtest/gtest.h>

#include <array>
#include <cerrno>
#include <cstdlib>
#include <cstring>
#include <fcntl.h>
#include <linux/fs.h>
#include <linux/loop.h>
#include <string>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/uio.h>
#include <unistd.h>
#include <vector>

namespace {
constexpr size_t kCapacity = 4096;

class Fd {
public:
    explicit Fd(int value = -1) : value_(value) {}
    ~Fd() { if (value_ >= 0) close(value_); }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int get() const { return value_; }
    void reset(int value = -1) { if (value_ >= 0) close(value_); value_ = value; }
private:
    int value_;
};

class Mapping {
public:
    Mapping() : length_(sysconf(_SC_PAGESIZE)),
        value_(mmap(nullptr, length_, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0)) {}
    ~Mapping() { if (value_ != MAP_FAILED) munmap(value_, length_); }
    void* get() const { return value_; }
private:
    size_t length_;
    void* value_;
};

enum class Entry { Read, Pread, Readv, Preadv };

ssize_t Receive(int fd, Entry entry, void* buffer, size_t size, off_t offset) {
    // The descriptor array is always accessible. Only payload may be NULL or
    // PROT_NONE, so this checks EOF before payload copy, not invalid iov import.
    iovec vectors[] = {{nullptr, 0}, {buffer, size}};
    switch (entry) {
        case Entry::Read: return read(fd, buffer, size);
        case Entry::Pread: return pread(fd, buffer, size, offset);
        case Entry::Readv: return readv(fd, vectors, 2);
        case Entry::Preadv: return preadv(fd, vectors, 2, offset);
    }
    return -1;
}

class LoopLifecycleIo : public testing::Test {
protected:
    void SetUp() override {
        const char* directory = getenv("DUNITEST_LOOP_BACKING_DIR");
        std::string pattern = std::string(directory ? directory : "/tmp") + "/loop_lifecycle_XXXXXX";
        std::vector<char> path(pattern.begin(), pattern.end());
        path.push_back('\0');
        backing_.reset(mkstemp(path.data()));
        ASSERT_GE(backing_.get(), 0) << strerror(errno);
        backing_path_ = path.data();
        ASSERT_EQ(0, ftruncate(backing_.get(), kCapacity));
        std::array<char, kCapacity> data;
        data.fill('a');
        ASSERT_EQ(static_cast<ssize_t>(data.size()), pwrite(backing_.get(), data.data(), data.size(), 0));
        ASSERT_EQ(0, fsync(backing_.get()));
        Fd control(open("/dev/loop-control", O_RDWR | O_CLOEXEC));
        ASSERT_GE(control.get(), 0);
        const int minor = ioctl(control.get(), LOOP_CTL_GET_FREE, 0);
        ASSERT_GE(minor, 0) << strerror(errno);
        loop_path_ = "/dev/loop" + std::to_string(minor);
        loop_.reset(open(loop_path_.c_str(), O_RDWR | O_CLOEXEC));
        ASSERT_GE(loop_.get(), 0) << strerror(errno);
    }

    void TearDown() override {
        // Tests can deliberately leave a final-opener AUTOCLEAR in flight.
        // CLR on an already unbound loop is ENXIO and has no cleanup work.
        if (loop_.get() >= 0) {
            const int result = ioctl(loop_.get(), LOOP_CLR_FD, 0);
            EXPECT_TRUE(result == 0 || errno == ENXIO) << strerror(errno);
        }
        loop_.reset();
        backing_.reset();
        if (!backing_path_.empty()) EXPECT_EQ(0, unlink(backing_path_.c_str()));
    }

    void Bind() { ASSERT_EQ(0, ioctl(loop_.get(), LOOP_SET_FD, backing_.get())) << strerror(errno); }
    void Capacity(uint64_t expected) {
        uint64_t bytes = UINT64_MAX;
        ASSERT_EQ(0, ioctl(loop_.get(), BLKGETSIZE64, &bytes)) << strerror(errno);
        ASSERT_EQ(expected, bytes);
    }
    void ExpectUnbound() {
        ASSERT_NO_FATAL_FAILURE(Capacity(0));
        loop_info64 status {};
        errno = 0;
        EXPECT_EQ(-1, ioctl(loop_.get(), LOOP_GET_STATUS64, &status));
        EXPECT_EQ(ENXIO, errno);
    }
    void ClearAndWait() {
        // A host device probe can temporarily be an additional opener. Linux
        // then converts CLR_FD to AUTOCLEAR; retry once that opener leaves.
        for (unsigned attempt = 0; attempt < 2000; ++attempt) {
            errno = 0;
            const int cleared = ioctl(loop_.get(), LOOP_CLR_FD, 0);
            const int clear_error = errno;
            ASSERT_TRUE(cleared == 0 || clear_error == ENXIO) << strerror(clear_error);
            loop_info64 status {};
            errno = 0;
            const int result = ioctl(loop_.get(), LOOP_GET_STATUS64, &status);
            const int error = errno;
            if (result == -1 && error == ENXIO) return;
            ASSERT_EQ(0, result) << strerror(error);
            ASSERT_NE(0U, status.lo_flags & LO_FLAGS_AUTOCLEAR);
            usleep(1000);
        }
        FAIL() << "explicit clear did not finish after transient openers left";
    }
    void ReopenAfterLastClose() {
        // Linux may defer final block-file release. Observe completion using
        // status, closing each probe so it cannot keep AUTOCLEAR busy itself.
        for (unsigned attempt = 0; attempt < 2000; ++attempt) {
            loop_.reset(open(loop_path_.c_str(), O_RDWR | O_CLOEXEC));
            ASSERT_GE(loop_.get(), 0) << strerror(errno);
            loop_info64 status {};
            const int result = ioctl(loop_.get(), LOOP_GET_STATUS64, &status);
            const int error = errno;
            if (result == -1 && error == ENXIO) return;
            ASSERT_EQ(0, result) << strerror(error);
            loop_.reset();
            usleep(1000);
        }
        FAIL() << "AUTOCLEAR did not complete after the last opener closed";
    }
    void ExpectByte(int fd, off_t offset, char expected) {
        char byte = '!';
        ASSERT_EQ(1, pread(fd, &byte, 1, offset)) << strerror(errno);
        EXPECT_EQ(expected, byte);
    }
    void EofMatrix(int fd) {
        Mapping unavailable;
        ASSERT_NE(MAP_FAILED, unavailable.get());
        std::array<char, kCapacity> output;
        for (Entry entry : {Entry::Read, Entry::Pread, Entry::Readv, Entry::Preadv}) {
            for (size_t size : {size_t(0), size_t(1), kCapacity}) {
                for (void* address : {static_cast<void*>(output.data()), static_cast<void*>(nullptr), unavailable.get()}) {
                    for (off_t offset : {off_t(0), off_t(137), off_t(2 * kCapacity)}) {
                        SCOPED_TRACE(static_cast<int>(entry));
                        SCOPED_TRACE(size);
                        SCOPED_TRACE(offset);
                        // Linux block llseek rejects positions beyond a zero
                        // capacity. Positional I/O can still request those EOFs.
                        const bool sequential = entry == Entry::Read || entry == Entry::Readv;
                        if (sequential && offset != 0) continue;
                        output.fill('!');
                        ASSERT_EQ(0, lseek(fd, 0, SEEK_SET));
                        errno = 0;
                        EXPECT_EQ(0, Receive(fd, entry, address, size, offset)) << strerror(errno);
                        EXPECT_EQ(0, lseek(fd, 0, SEEK_CUR));
                        EXPECT_EQ('!', output.front());
                        EXPECT_EQ('!', output.back());
                    }
                }
            }
        }
    }

    Fd backing_, loop_;
    std::string backing_path_, loop_path_;
};

TEST_F(LoopLifecycleIo, NeverBoundReadsAreEofAcrossAllEntriesAndPayloads) {
    ASSERT_NO_FATAL_FAILURE(ExpectUnbound());
    ASSERT_NO_FATAL_FAILURE(EofMatrix(loop_.get()));
}

TEST_F(LoopLifecycleIo, ExplicitClearMakesExistingDescriptionReadEof) {
    ASSERT_NO_FATAL_FAILURE(Bind());
    ASSERT_NO_FATAL_FAILURE(Capacity(kCapacity));
    ASSERT_NO_FATAL_FAILURE(ExpectByte(loop_.get(), 0, 'a'));
    ASSERT_NO_FATAL_FAILURE(ClearAndWait());
    ASSERT_NO_FATAL_FAILURE(ExpectUnbound());
    ASSERT_NO_FATAL_FAILURE(EofMatrix(loop_.get()));
}

TEST_F(LoopLifecycleIo, BusyClearRetainsMappingUntilTheLastOpenerCloses) {
    ASSERT_NO_FATAL_FAILURE(Bind());
    Fd second(open(loop_path_.c_str(), O_RDWR | O_CLOEXEC));
    ASSERT_GE(second.get(), 0);
    ASSERT_EQ(0, ioctl(loop_.get(), LOOP_CLR_FD, 0));
    loop_info64 status {};
    ASSERT_EQ(0, ioctl(loop_.get(), LOOP_GET_STATUS64, &status));
    EXPECT_NE(0U, status.lo_flags & LO_FLAGS_AUTOCLEAR);
    ASSERT_NO_FATAL_FAILURE(Capacity(kCapacity));
    ASSERT_NO_FATAL_FAILURE(ExpectByte(second.get(), 0, 'a'));
    second.reset();
    ASSERT_NO_FATAL_FAILURE(ExpectByte(loop_.get(), 0, 'a'));
    loop_.reset();
    ASSERT_NO_FATAL_FAILURE(ReopenAfterLastClose());
    ASSERT_NO_FATAL_FAILURE(ExpectUnbound());
    ASSERT_NO_FATAL_FAILURE(EofMatrix(loop_.get()));
}

TEST_F(LoopLifecycleIo, ExplicitAutoclearAlsoWaitsForLastOpenDescription) {
    ASSERT_NO_FATAL_FAILURE(Bind());
    Fd second(open(loop_path_.c_str(), O_RDWR | O_CLOEXEC));
    ASSERT_GE(second.get(), 0);
    loop_info64 status {};
    status.lo_flags = LO_FLAGS_AUTOCLEAR;
    ASSERT_EQ(0, ioctl(loop_.get(), LOOP_SET_STATUS64, &status));
    loop_.reset();
    ASSERT_NO_FATAL_FAILURE(ExpectByte(second.get(), 0, 'a'));
    second.reset();
    ASSERT_NO_FATAL_FAILURE(ReopenAfterLastClose());
    ASSERT_NO_FATAL_FAILURE(ExpectUnbound());
    ASSERT_NO_FATAL_FAILURE(EofMatrix(loop_.get()));
}

TEST_F(LoopLifecycleIo, RebindingTheSameDescriptionPublishesTheNewCapacityAndData) {
    ASSERT_NO_FATAL_FAILURE(Bind());
    ASSERT_NO_FATAL_FAILURE(ClearAndWait());
    ASSERT_NO_FATAL_FAILURE(ExpectUnbound());
    ASSERT_EQ(0, ftruncate(backing_.get(), 2 * kCapacity));
    const char byte = 'b';
    ASSERT_EQ(1, pwrite(backing_.get(), &byte, 1, kCapacity));
    ASSERT_EQ(0, fsync(backing_.get()));
    ASSERT_NO_FATAL_FAILURE(Bind());
    ASSERT_NO_FATAL_FAILURE(Capacity(2 * kCapacity));
    ASSERT_NO_FATAL_FAILURE(ExpectByte(loop_.get(), 0, 'a'));
    ASSERT_NO_FATAL_FAILURE(ExpectByte(loop_.get(), kCapacity, 'b'));
    for (Entry entry : {Entry::Read, Entry::Pread, Entry::Readv, Entry::Preadv}) {
        char actual = '!';
        ASSERT_EQ(static_cast<off_t>(kCapacity), lseek(loop_.get(), kCapacity, SEEK_SET));
        ASSERT_EQ(1, Receive(loop_.get(), entry, &actual, 1, kCapacity));
        EXPECT_EQ('b', actual);
    }
}

TEST_F(LoopLifecycleIo, DirectEofPrecedesPayloadAndAlignmentChecks) {
    Fd direct(open(loop_path_.c_str(), O_RDWR | O_DIRECT | O_CLOEXEC));
    ASSERT_GE(direct.get(), 0);
    ASSERT_NO_FATAL_FAILURE(EofMatrix(direct.get()));
    ASSERT_NO_FATAL_FAILURE(Bind());
    void* aligned = nullptr;
    ASSERT_EQ(0, posix_memalign(&aligned, 512, 512));
    const ssize_t count = pread(direct.get(), aligned, 512, 0);
    EXPECT_EQ(512, count) << strerror(errno);
    if (count == 512) { EXPECT_EQ('a', static_cast<char*>(aligned)[0]); }
    free(aligned);
    char byte;
    errno = 0;
    EXPECT_EQ(-1, pread(direct.get(), &byte, 1, 0));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(0, pread(direct.get(), nullptr, 1, kCapacity));
}

TEST_F(LoopLifecycleIo, BoundZeroCapacityIsEofWithoutBecomingUnbound) {
    ASSERT_NO_FATAL_FAILURE(Bind());
    loop_info64 status {};
    status.lo_sizelimit = 1;
    ASSERT_EQ(0, ioctl(loop_.get(), LOOP_SET_STATUS64, &status));
    ASSERT_NO_FATAL_FAILURE(Capacity(0));
    ASSERT_EQ(0, ioctl(loop_.get(), LOOP_GET_STATUS64, &status));
    ASSERT_NO_FATAL_FAILURE(EofMatrix(loop_.get()));
}

TEST_F(LoopLifecycleIo, UnboundWriteAndSyncKeepTheirOwnLinuxBoundaries) {
    Mapping unavailable;
    ASSERT_NE(MAP_FAILED, unavailable.get());
    char byte = 'x';
    for (void* address : {static_cast<void*>(&byte), static_cast<void*>(nullptr), unavailable.get()}) {
        iovec vector {address, 1};
        for (int entry = 0; entry < 4; ++entry) {
            errno = 0;
            ssize_t count;
            switch (entry) {
                case 0: count = write(loop_.get(), address, 1); break;
                case 1: count = pwrite(loop_.get(), address, 1, 0); break;
                case 2: count = writev(loop_.get(), &vector, 1); break;
                default: count = pwritev(loop_.get(), &vector, 1, 0); break;
            }
            EXPECT_EQ(-1, count);
            EXPECT_EQ(ENOSPC, errno);
        }
        EXPECT_EQ(0, write(loop_.get(), address, 0));
        EXPECT_EQ(0, pwrite(loop_.get(), address, 0, 0));
    }
    EXPECT_EQ(0, fdatasync(loop_.get())) << strerror(errno);
    EXPECT_EQ(0, fsync(loop_.get())) << strerror(errno);
}

} // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
