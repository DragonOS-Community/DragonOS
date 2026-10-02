#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#include <string>

namespace {

size_t PageSize() {
    const long ps = sysconf(_SC_PAGESIZE);
    return ps > 0 ? static_cast<size_t>(ps) : 4096;
}

class TempFile {
  public:
    TempFile() {
        char tmpl[] = "/tmp/dunitest_mmap_truncate_cow_XXXXXX";
        fd_ = mkstemp(tmpl);
        if (fd_ >= 0) {
            path_ = tmpl;
        }
    }

    ~TempFile() {
        if (fd_ >= 0) {
            close(fd_);
        }
        if (!path_.empty()) {
            unlink(path_.c_str());
        }
    }

    TempFile(const TempFile&) = delete;
    TempFile& operator=(const TempFile&) = delete;

    bool valid() const {
        return fd_ >= 0;
    }

    int fd() const {
        return fd_;
    }

  private:
    std::string path_;
    int fd_ = -1;
};

void ExpectChildDiesBySignal(int signal, void (*fn)()) {
    const pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";

    if (child == 0) {
        fn();
        _exit(0);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0))
        << "waitpid failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_TRUE(WIFSIGNALED(status)) << "child exited without signal, status=" << status;
    EXPECT_EQ(signal, WTERMSIG(status)) << "unexpected signal, status=" << status;
}

volatile char* g_mapping = nullptr;

void ReadMappedByte() {
    const volatile char byte = g_mapping[0];
    (void)byte;
}

}  // namespace

TEST(MmapTruncateCow, PrivateCowPageIsInvalidatedAfterTruncateToZero) {
    const size_t ps = PageSize();
    TempFile file;
    ASSERT_TRUE(file.valid()) << "mkstemp failed: errno=" << errno << " (" << strerror(errno)
                              << ")";
    ASSERT_EQ(0, ftruncate(file.fd(), static_cast<off_t>(ps)))
        << "ftruncate to page failed: errno=" << errno << " (" << strerror(errno) << ")";

    void* mapping = mmap(nullptr, ps, PROT_READ | PROT_WRITE, MAP_PRIVATE, file.fd(), 0);
    ASSERT_NE(MAP_FAILED, mapping) << "mmap failed: errno=" << errno << " (" << strerror(errno)
                                   << ")";

    memset(mapping, 'a', ps);
    ASSERT_EQ(0, ftruncate(file.fd(), 0))
        << "ftruncate to zero failed: errno=" << errno << " (" << strerror(errno) << ")";

    g_mapping = static_cast<volatile char*>(mapping);
    ExpectChildDiesBySignal(SIGBUS, ReadMappedByte);

    ASSERT_EQ(0, munmap(mapping, ps)) << "munmap failed: errno=" << errno << " ("
                                      << strerror(errno) << ")";
    g_mapping = nullptr;
}

TEST(MmapTruncateCow, PartialPageTruncateKeepsContainingPageAndInvalidatesFollowingCowPage) {
    const size_t ps = PageSize();
    TempFile file;
    ASSERT_TRUE(file.valid()) << "mkstemp failed: errno=" << errno << " (" << strerror(errno)
                              << ")";
    ASSERT_EQ(0, ftruncate(file.fd(), static_cast<off_t>(ps * 2)))
        << "ftruncate to two pages failed: errno=" << errno << " (" << strerror(errno) << ")";

    void* mapping = mmap(nullptr, ps * 2, PROT_READ | PROT_WRITE, MAP_PRIVATE, file.fd(), 0);
    ASSERT_NE(MAP_FAILED, mapping) << "mmap failed: errno=" << errno << " (" << strerror(errno)
                                   << ")";

    memset(mapping, 'b', ps * 2);
    ASSERT_EQ(0, ftruncate(file.fd(), static_cast<off_t>(ps / 2)))
        << "partial ftruncate failed: errno=" << errno << " (" << strerror(errno) << ")";

    auto* bytes = static_cast<volatile char*>(mapping);
    EXPECT_EQ('b', bytes[0]);

    g_mapping = bytes + ps;
    ExpectChildDiesBySignal(SIGBUS, ReadMappedByte);

    ASSERT_EQ(0, munmap(mapping, ps * 2)) << "munmap failed: errno=" << errno << " ("
                                          << strerror(errno) << ")";
    g_mapping = nullptr;
}

TEST(MmapTruncateCow, LastPrivateFileCowMappingSurvivesParentUnmap) {
    // The child inherits an anonymous COW page in a file-backed VMA. After
    // parent unmap, its write replaces the last mapping of that old page.
    const size_t ps = PageSize();
    const size_t length = ps * 256;
    TempFile file;
    ASSERT_TRUE(file.valid());
    ASSERT_EQ(0, ftruncate(file.fd(), length));
    auto* bytes = static_cast<volatile char*>(
        mmap(nullptr, length, PROT_READ | PROT_WRITE, MAP_PRIVATE, file.fd(), 0));
    ASSERT_NE(MAP_FAILED, const_cast<char*>(bytes));
    for (size_t offset = 0; offset < length; offset += ps) {
        bytes[offset] = 'a';
    }
    int ready[2];
    ASSERT_EQ(0, pipe(ready));
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(ready[1]);
        char token;
        if (read(ready[0], &token, 1) != 1) _exit(91);
        close(ready[0]);
        for (size_t offset = 0; offset < length; offset += ps) {
            if (bytes[offset] != 'a') _exit(92);
            bytes[offset] = 'b';
            if (bytes[offset] != 'b') _exit(93);
        }
        if (munmap(const_cast<char*>(bytes), length) != 0) _exit(94);
        _exit(0);
    }
    close(ready[0]);
    EXPECT_EQ(0, munmap(const_cast<char*>(bytes), length));
    EXPECT_EQ(1, write(ready[1], "x", 1));
    close(ready[1]);
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    char byte = 1;
    ASSERT_EQ(1, pread(file.fd(), &byte, 1, 0));
    EXPECT_EQ(0, byte);  // Neither private write may change the backing file.
}

TEST(MmapTruncateCow, AnonymousChildCowDoesNotInheritParentMlock) {
    const size_t length = PageSize() * 4;
    auto* bytes = static_cast<volatile char*>(
        mmap(nullptr, length, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, const_cast<char*>(bytes));
    ASSERT_EQ(0, mlock(const_cast<char*>(bytes), length));
    bytes[0] = 'p';
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        // Linux does not inherit VM_LOCKED across fork. This COW page must
        // acquire the child's policy, not the locked source page's policy.
        if (bytes[0] != 'p') _exit(95);
        for (size_t offset = 0; offset < length; offset += PageSize()) {
            bytes[offset] = 'c';
        }
        if (munmap(const_cast<char*>(bytes), length) != 0) _exit(96);
        _exit(0);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    EXPECT_EQ('p', bytes[0]);
    EXPECT_EQ(0, munlock(const_cast<char*>(bytes), length));
    EXPECT_EQ(0, munmap(const_cast<char*>(bytes), length));
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
