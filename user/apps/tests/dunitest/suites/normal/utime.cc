#include <gtest/gtest.h>

#include <cerrno>
#include <cstdlib>
#include <cstring>
#include <ctime>
#include <fcntl.h>
#include <string>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <unistd.h>
#include <utime.h>

#if defined(SYS_utime)
namespace {

class UtimeTest : public ::testing::Test {
protected:
    void SetUp() override {
        char path[] = "/tmp/dunitest_utime_XXXXXX";
        ASSERT_NE(nullptr, mkdtemp(path));
        dir_ = path;
        const int fd = open(Path("file").c_str(), O_CREAT | O_EXCL | O_RDWR, 0600);
        ASSERT_GE(fd, 0);
        ASSERT_EQ(0, close(fd));
    }

    void TearDown() override {
        if (dir_.empty()) return;
        unlink(Path("link").c_str());
        unlink(Path("file").c_str());
        rmdir(dir_.c_str());
    }

    std::string Path(const char* name) const { return dir_ + "/" + name; }

    long SetTimes(const char* path, const struct utimbuf* times) {
        return syscall(SYS_utime, path, times);
    }

    std::string dir_;
};

TEST_F(UtimeTest, ExplicitSecondsClearNanoseconds) {
    const struct timespec initial[2] = {{1600000000, 123456789}, {1600000001, 987654321}};
    ASSERT_EQ(0, utimensat(AT_FDCWD, Path("file").c_str(), initial, 0));
    const struct utimbuf times = {1700000010, 1700000020};
    ASSERT_EQ(0, SetTimes(Path("file").c_str(), &times));
    struct stat st = {};
    ASSERT_EQ(0, stat(Path("file").c_str(), &st));
    EXPECT_EQ(times.actime, st.st_atim.tv_sec);
    EXPECT_EQ(0, st.st_atim.tv_nsec);
    EXPECT_EQ(times.modtime, st.st_mtim.tv_sec);
    EXPECT_EQ(0, st.st_mtim.tv_nsec);
}

TEST_F(UtimeTest, NullTimesUseCurrentTime) {
    const struct utimbuf old = {100, 200};
    ASSERT_EQ(0, SetTimes(Path("file").c_str(), &old));
    struct timespec before = {}, after = {};
    ASSERT_EQ(0, clock_gettime(CLOCK_REALTIME, &before));
    ASSERT_EQ(0, SetTimes(Path("file").c_str(), nullptr));
    ASSERT_EQ(0, clock_gettime(CLOCK_REALTIME, &after));
    struct stat st = {};
    ASSERT_EQ(0, stat(Path("file").c_str(), &st));
    EXPECT_GE(st.st_atim.tv_sec, before.tv_sec);
    EXPECT_LE(st.st_atim.tv_sec, after.tv_sec);
    EXPECT_GE(st.st_mtim.tv_sec, before.tv_sec);
    EXPECT_LE(st.st_mtim.tv_sec, after.tv_sec);
    EXPECT_EQ(st.st_atim.tv_sec, st.st_mtim.tv_sec);
    EXPECT_EQ(st.st_atim.tv_nsec, st.st_mtim.tv_nsec);
}

TEST_F(UtimeTest, FinalSymlinkUpdatesTarget) {
    ASSERT_EQ(0, symlink("file", Path("link").c_str()));
    const struct timespec link_times[2] = {{2100000000, 123}, {2100000001, 456}};
    ASSERT_EQ(0, utimensat(AT_FDCWD, Path("link").c_str(), link_times, AT_SYMLINK_NOFOLLOW));
    struct stat before = {}, after = {}, target = {};
    ASSERT_EQ(0, lstat(Path("link").c_str(), &before));
    const struct utimbuf times = {1700000030, 1700000040};
    ASSERT_EQ(0, SetTimes(Path("link").c_str(), &times));
    ASSERT_EQ(0, lstat(Path("link").c_str(), &after));
    // Following a symlink may update its atime on Linux; utime must leave
    // the link's explicitly set mtime and metadata-change time untouched.
    EXPECT_EQ(before.st_ctim.tv_sec, after.st_ctim.tv_sec);
    EXPECT_EQ(before.st_ctim.tv_nsec, after.st_ctim.tv_nsec);
    EXPECT_EQ(before.st_mtim.tv_sec, after.st_mtim.tv_sec);
    EXPECT_EQ(before.st_mtim.tv_nsec, after.st_mtim.tv_nsec);
    ASSERT_EQ(0, stat(Path("file").c_str(), &target));
    EXPECT_EQ(times.actime, target.st_atim.tv_sec);
    EXPECT_EQ(0, target.st_atim.tv_nsec);
    EXPECT_EQ(times.modtime, target.st_mtim.tv_sec);
    EXPECT_EQ(0, target.st_mtim.tv_nsec);
}

TEST_F(UtimeTest, InvalidTimesPointerFailsWithoutMutation) {
    const struct utimbuf times = {1700000050, 1700000060};
    ASSERT_EQ(0, SetTimes(Path("file").c_str(), &times));
    errno = 0;
    EXPECT_EQ(-1, SetTimes(Path("file").c_str(), reinterpret_cast<const struct utimbuf*>(1)));
    EXPECT_EQ(EFAULT, errno);
    struct stat st = {};
    ASSERT_EQ(0, stat(Path("file").c_str(), &st));
    EXPECT_EQ(times.actime, st.st_atim.tv_sec);
    EXPECT_EQ(times.modtime, st.st_mtim.tv_sec);
}

TEST_F(UtimeTest, UnalignedTimesPointerWorks) {
    const struct utimbuf times = {1700000090, 1700000100};
    alignas(struct utimbuf) unsigned char storage[sizeof(times) + 1] = {};
    std::memcpy(storage + 1, &times, sizeof(times));
    ASSERT_EQ(0, SetTimes(Path("file").c_str(),
                         reinterpret_cast<const struct utimbuf*>(storage + 1)));
    struct stat st = {};
    ASSERT_EQ(0, stat(Path("file").c_str(), &st));
    EXPECT_EQ(times.actime, st.st_atim.tv_sec);
    EXPECT_EQ(times.modtime, st.st_mtim.tv_sec);
}

TEST_F(UtimeTest, UnreadableAndCrossPageTimesFailWithoutMutation) {
    const struct utimbuf times = {1700000110, 1700000120};
    ASSERT_EQ(0, SetTimes(Path("file").c_str(), &times));
    const long page_size = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page_size, static_cast<long>(sizeof(times)));
    void* mapping = mmap(nullptr, 2 * page_size, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, mapping);
    auto* second_page = static_cast<unsigned char*>(mapping) + page_size;
    auto* crossing = second_page - sizeof(times) / 2;
    std::memcpy(crossing, &times, sizeof(times));
    const int protected_result = mprotect(second_page, page_size, PROT_NONE);
    EXPECT_EQ(0, protected_result);
    if (protected_result == 0) {
        errno = 0;
        EXPECT_EQ(-1, SetTimes(Path("file").c_str(),
                               reinterpret_cast<const struct utimbuf*>(second_page)));
        EXPECT_EQ(EFAULT, errno);
        errno = 0;
        EXPECT_EQ(-1, SetTimes(Path("file").c_str(),
                               reinterpret_cast<const struct utimbuf*>(crossing)));
        EXPECT_EQ(EFAULT, errno);
    }
    EXPECT_EQ(0, munmap(mapping, 2 * page_size));
    struct stat st = {};
    ASSERT_EQ(0, stat(Path("file").c_str(), &st));
    EXPECT_EQ(times.actime, st.st_atim.tv_sec);
    EXPECT_EQ(times.modtime, st.st_mtim.tv_sec);
}

TEST_F(UtimeTest, NullMissingAndEmptyPaths) {
    const struct utimbuf times = {1700000070, 1700000080};
    errno = 0;
    EXPECT_EQ(-1, SetTimes(nullptr, &times));
    EXPECT_EQ(EFAULT, errno);
    errno = 0;
    EXPECT_EQ(-1, SetTimes(Path("missing").c_str(), &times));
    EXPECT_EQ(ENOENT, errno);
    errno = 0;
    EXPECT_EQ(-1, SetTimes("", &times));
    EXPECT_EQ(ENOENT, errno);
}

TEST_F(UtimeTest, TmpfsAcceptsNegativeSeconds) {
    constexpr long kTmpfsMagic = 0x01021994;
    std::string base;
    for (const char* candidate : {"/tmp", "/dev/shm"}) {
        struct statfs fs = {};
        if (statfs(candidate, &fs) == 0 && fs.f_type == kTmpfsMagic) {
            base = candidate;
            break;
        }
    }
    if (base.empty()) GTEST_SKIP() << "No writable tmpfs test directory";
    std::string path = base + "/dunitest_utime_negative_XXXXXX";
    const int fd = mkstemp(&path[0]);
    ASSERT_GE(fd, 0);
    EXPECT_EQ(0, close(fd));
    const struct utimbuf times = {-10, -20};
    errno = 0;
    const long result = SetTimes(path.c_str(), &times);
    const int saved_errno = errno;
    struct stat st = {};
    const int stat_result = stat(path.c_str(), &st);
    EXPECT_EQ(0, unlink(path.c_str()));
    ASSERT_EQ(0, result) << saved_errno;
    ASSERT_EQ(0, stat_result);
    EXPECT_EQ(times.actime, st.st_atim.tv_sec);
    EXPECT_EQ(0, st.st_atim.tv_nsec);
    EXPECT_EQ(times.modtime, st.st_mtim.tv_sec);
    EXPECT_EQ(0, st.st_mtim.tv_nsec);
}

}  // namespace
#else
TEST(UtimeTest, UnavailableOnThisArchitecture) {
    GTEST_SKIP() << "This architecture has no SYS_utime";
}
#endif
