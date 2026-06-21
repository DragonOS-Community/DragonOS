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

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
