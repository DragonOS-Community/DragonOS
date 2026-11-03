#include <gtest/gtest.h>

#include <array>
#include <cerrno>
#include <cstring>
#include <fcntl.h>
#include <dirent.h>
#include <map>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <string>
#include <unistd.h>

namespace {

std::string ReadFdLink(int fd, size_t buf_size = 128) {
    std::string proc_path = "/proc/self/fd/" + std::to_string(fd);
    std::string target(buf_size, '\0');
    ssize_t len = readlink(proc_path.c_str(), target.data(), target.size());
    if (len < 0) {
        return "";
    }
    target.resize(static_cast<size_t>(len));
    return target;
}

void ExpectProcFdTarget(const char* path) {
    int fd = open(path, O_RDONLY);
    ASSERT_GE(fd, 0) << "open(" << path << ") failed: errno=" << errno << " ("
                     << std::strerror(errno) << ")";

    EXPECT_EQ(path, ReadFdLink(fd));
    EXPECT_EQ(std::string(path).substr(0, 1), ReadFdLink(fd, 1));

    std::string proc_path = "/proc/self/fd/" + std::to_string(fd);
    std::array<char, 1> dummy {};
    errno = 0;
    // Some libc wrappers short-circuit a zero-length readlink. Exercise the kernel.
    ssize_t len = syscall(SYS_readlink, proc_path.c_str(), dummy.data(), 0);
    EXPECT_EQ(-1, len);
    EXPECT_EQ(EINVAL, errno) << "zero-sized readlink failed with errno=" << errno
                             << " (" << std::strerror(errno) << ")";

    EXPECT_EQ(0, close(fd));
}

void ExpectCharDeviceNumber(const char* path, unsigned int major_num, unsigned int minor_num) {
    struct stat st = {};
    ASSERT_EQ(0, stat(path, &st)) << "stat(" << path << ") failed: errno=" << errno << " ("
                                  << std::strerror(errno) << ")";

    EXPECT_TRUE(S_ISCHR(st.st_mode)) << path << " is not a character device";
    EXPECT_EQ(major_num, major(st.st_rdev)) << path << " major mismatch";
    EXPECT_EQ(minor_num, minor(st.st_rdev)) << path << " minor mismatch";
}

void ExpectReadZeros(const char* path) {
    int fd = open(path, O_RDONLY);
    ASSERT_GE(fd, 0) << "open(" << path << ") failed: errno=" << errno << " ("
                     << std::strerror(errno) << ")";

    std::array<unsigned char, 16> buf {};
    buf.fill(0xff);
    ASSERT_EQ(static_cast<ssize_t>(buf.size()), read(fd, buf.data(), buf.size()))
        << "read(" << path << ") failed: errno=" << errno << " (" << std::strerror(errno)
        << ")";
    for (unsigned char byte : buf) {
        EXPECT_EQ(0, byte);
    }

    EXPECT_EQ(0, close(fd));
}

}  // namespace

TEST(ProcFdDevfsReadlink, BuiltinCharacterDevicesResolveToDevPaths) {
    ExpectProcFdTarget("/dev/null");
    ExpectProcFdTarget("/dev/zero");
    ExpectProcFdTarget("/dev/full");
    ExpectProcFdTarget("/dev/random");
    ExpectProcFdTarget("/dev/urandom");
}

TEST(ProcFdDevfsReadlink, BuiltinCharacterDevicesExposeLinuxDeviceNumbers) {
    ExpectCharDeviceNumber("/dev/null", 1, 3);
    ExpectCharDeviceNumber("/dev/zero", 1, 5);
    ExpectCharDeviceNumber("/dev/full", 1, 7);
    ExpectCharDeviceNumber("/dev/random", 1, 8);
    ExpectCharDeviceNumber("/dev/urandom", 1, 9);
}

TEST(ProcFdDevfsReadlink, FullDeviceMatchesLinuxReadWriteSemantics) {
    ExpectReadZeros("/dev/full");

    int fd = open("/dev/full", O_WRONLY);
    ASSERT_GE(fd, 0) << "open(/dev/full) failed: errno=" << errno << " ("
                     << std::strerror(errno) << ")";

    const char data[] = "x";
    errno = 0;
    ssize_t written = write(fd, data, sizeof(data));
    EXPECT_EQ(-1, written);
    EXPECT_EQ(ENOSPC, errno) << "write(/dev/full) failed with errno=" << errno << " ("
                             << std::strerror(errno) << ")";

    EXPECT_EQ(0, close(fd));
}

namespace {

class ProcCacheFd {
 public:
    explicit ProcCacheFd(int fd) : fd_(fd) {}
    ~ProcCacheFd() { if (fd_ >= 0) close(fd_); }
    ProcCacheFd(const ProcCacheFd&) = delete;
    ProcCacheFd& operator=(const ProcCacheFd&) = delete;
    int get() const { return fd_; }
    int Close() {
        const int fd = fd_;
        fd_ = -1;
        return close(fd);
    }
 private:
    int fd_;
};

// Reopening starts a new directory stream. Keep the watched slot well above
// these temporary descriptors so enumeration cannot itself refill that slot.
bool ScanProcDirectory(const char* path, std::map<std::string, ino_t>* entries) {
    DIR* dir = opendir(path);
    if (dir == nullptr) return false;
    entries->clear();
    errno = 0;
    while (dirent* entry = readdir(dir)) {
        (*entries)[entry->d_name] = entry->d_ino;
        errno = 0;
    }
    const int error = errno;
    const int result = closedir(dir);
    if (error != 0) errno = error;
    return error == 0 && result == 0;
}

// Raw reads plus rewind exercise the live fdinfo lookup, independently of any
// userspace stdio buffering. Do not require particular optional fdinfo fields.
bool ReadCurrentFdInfo(int fd) {
    if (lseek(fd, 0, SEEK_SET) != 0) return false;
    char buffer[512];
    ssize_t n;
    do { n = read(fd, buffer, sizeof(buffer)); } while (n > 0);
    return n == 0;
}

}  // namespace

TEST(ProcFdCache, LiveSlotIdentitySurvivesRepeatedEnumeration) {
    ProcCacheFd original(open("/dev/null", O_RDONLY | O_CLOEXEC));
    ASSERT_GE(original.get(), 0);
    ProcCacheFd watched(fcntl(original.get(), F_DUPFD_CLOEXEC, 32));
    ASSERT_GE(watched.get(), 32);
    const std::string name = std::to_string(watched.get());

    for (const char* directory : {"/proc/self/fd", "/proc/self/fdinfo"}) {
        SCOPED_TRACE(directory);
        const std::string path = std::string(directory) + "/" + name;
        // Pin the observed inode: identity comparisons must not depend on
        // whether the kernel reclaims an otherwise unreferenced proc inode.
        ProcCacheFd pin(open(path.c_str(), O_PATH | O_NOFOLLOW | O_CLOEXEC));
        ASSERT_GE(pin.get(), 0);
        struct stat before = {};
        ASSERT_EQ(0, fstat(pin.get(), &before));
        for (int iteration = 0; iteration < 4; ++iteration) {
            std::map<std::string, ino_t> entries;
            ASSERT_TRUE(ScanProcDirectory(directory, &entries)) << strerror(errno);
            ASSERT_EQ(1u, entries.count(name));
            EXPECT_EQ(before.st_ino, entries.at(name));
            struct stat after = {};
            ASSERT_EQ(0, lstat(path.c_str(), &after));
            EXPECT_EQ(before.st_dev, after.st_dev);
            EXPECT_EQ(before.st_ino, after.st_ino);
        }
    }
}

TEST(ProcFdCache, ClosedSlotDisappearsAndReusedSlotResolvesCurrentFile) {
    ProcCacheFd original(open("/dev/null", O_RDONLY | O_CLOEXEC));
    ProcCacheFd replacement(open("/dev/zero", O_RDONLY | O_CLOEXEC));
    ASSERT_GE(original.get(), 0);
    ASSERT_GE(replacement.get(), 0);
    ProcCacheFd watched(fcntl(original.get(), F_DUPFD_CLOEXEC, 32));
    ASSERT_GE(watched.get(), 32);
    const int slot = watched.get();
    const std::string name = std::to_string(slot);
    const std::string link = "/proc/self/fd/" + name;
    const std::string info = "/proc/self/fdinfo/" + name;
    ProcCacheFd link_pin(open(link.c_str(), O_PATH | O_NOFOLLOW | O_CLOEXEC));
    ProcCacheFd info_pin(open(info.c_str(), O_RDONLY | O_CLOEXEC));
    ASSERT_GE(link_pin.get(), 0);
    ASSERT_GE(info_pin.get(), 0);
    ASSERT_TRUE(ReadCurrentFdInfo(info_pin.get()));
    ASSERT_EQ("/dev/null", ReadFdLink(slot));
    for (const char* directory : {"/proc/self/fd", "/proc/self/fdinfo"}) {
        std::map<std::string, ino_t> entries;
        ASSERT_TRUE(ScanProcDirectory(directory, &entries));
        ASSERT_EQ(1u, entries.count(name));
    }
    // Leave a lower descriptor free for every temporary directory stream.
    ASSERT_LT(original.get(), slot);
    ASSERT_EQ(0, original.Close());
    ASSERT_EQ(0, watched.Close());
    for (const char* directory : {"/proc/self/fd", "/proc/self/fdinfo"}) {
        std::map<std::string, ino_t> entries;
        ASSERT_TRUE(ScanProcDirectory(directory, &entries));
        EXPECT_EQ(0u, entries.count(name));
    }
    char buffer[128];
    errno = 0;
    EXPECT_EQ(-1, readlinkat(link_pin.get(), "", buffer, sizeof(buffer)));
    EXPECT_EQ(ENOENT, errno);
    errno = 0;
    EXPECT_FALSE(ReadCurrentFdInfo(info_pin.get()));
    EXPECT_EQ(ENOENT, errno);

    const int reused_fd = dup2(replacement.get(), slot);
    ProcCacheFd reused(reused_fd);
    ASSERT_EQ(slot, reused_fd);
    EXPECT_EQ("/dev/zero", ReadFdLink(slot));
    const ssize_t len = readlinkat(link_pin.get(), "", buffer, sizeof(buffer));
    ASSERT_GE(len, 0);
    EXPECT_EQ("/dev/zero", std::string(buffer, static_cast<size_t>(len)));
    EXPECT_TRUE(ReadCurrentFdInfo(info_pin.get())) << strerror(errno);
    ProcCacheFd new_info(open(info.c_str(), O_RDONLY | O_CLOEXEC));
    ASSERT_GE(new_info.get(), 0);
    EXPECT_TRUE(ReadCurrentFdInfo(new_info.get()));
    for (const char* directory : {"/proc/self/fd", "/proc/self/fdinfo"}) {
        std::map<std::string, ino_t> entries;
        ASSERT_TRUE(ScanProcDirectory(directory, &entries));
        EXPECT_EQ(1u, entries.count(name));
    }
    // A slot removed and then recreated is not required to keep its inode id.
}

TEST(ProcFdCache, EnumerationDoesNotExposeNoncanonicalLookupNames) {
    ProcCacheFd watched(open("/dev/null", O_RDONLY | O_CLOEXEC));
    ASSERT_GE(watched.get(), 0);
    const std::string name = std::to_string(watched.get());
    for (const char* directory : {"/proc/self/fd", "/proc/self/fdinfo"}) {
        for (const std::string& alias : {"0" + name, "+" + name}) {
            SCOPED_TRACE(std::string(directory) + "/" + alias);
            // Linux rejects these lookups. Even a filesystem accepting one
            // must not expose cached lookup aliases as extra directory entries.
            const std::string path = std::string(directory) + "/" + alias;
            ProcCacheFd lookup(open(path.c_str(), O_PATH | O_NOFOLLOW | O_CLOEXEC));
            if (lookup.get() < 0) {
                ASSERT_EQ(ENOENT, errno);
            }
            std::map<std::string, ino_t> entries;
            ASSERT_TRUE(ScanProcDirectory(directory, &entries));
            EXPECT_EQ(1u, entries.count(name));
            EXPECT_EQ(0u, entries.count(alias));
        }
    }
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
