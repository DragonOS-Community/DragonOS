#include <gtest/gtest.h>
#include <cerrno>
#include <cstdint>
#include <cstring>
#include <fcntl.h>
#include <map>
#include <sched.h>
#include <string>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

namespace {
class Fd {
 public:
    explicit Fd(int value) : value_(value) {}
    ~Fd() { if (value_ >= 0) close(value_); }
    int get() const { return value_; }
 private:
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int value_;
};
struct Entry { uint64_t ino; unsigned char type; };
using Entries = std::map<std::string, Entry>;
// Raw syscall avoids libc readdir buffering past the mutation under test.
bool ReadBatch(int fd, Entries* entries, bool* eof, size_t size = 24) {
    char buffer[256];
    ssize_t count = syscall(SYS_getdents64, fd, buffer, size);
    if (count < 0) return false;
    *eof = count == 0;
    for (size_t offset = 0; offset < static_cast<size_t>(count);) {
        if (static_cast<size_t>(count) - offset < 20) return false;
        uint64_t ino;
        uint16_t reclen;
        memcpy(&ino, buffer + offset, sizeof(ino));
        memcpy(&reclen, buffer + offset + 16, sizeof(reclen));
        if (reclen < 20 || reclen > static_cast<size_t>(count) - offset) return false;
        const char* name = buffer + offset + 19;
        size_t len = strnlen(name, reclen - 19);
        if (len == static_cast<size_t>(reclen - 19)) return false;
        if (!entries->emplace(std::string(name, len), Entry{ino,
                static_cast<unsigned char>(buffer[offset + 18])}).second) return false;
        offset += reclen;
    }
    return true;
}
bool ReadRest(int fd, Entries* entries, size_t size = 256) {
    for (int i = 0; i < 4096; ++i) {
        bool eof;
        if (!ReadBatch(fd, entries, &eof, size)) return false;
        if (eof) return true;
    }
    return false;
}
class DirectoryEntryMetadata : public testing::Test {
 protected:
    void SetUp() override {
        // This process alone changes namespaces; nothing is mounted in the
        // runner's original namespace, even when an assertion fails.
        const int result = unshare(CLONE_NEWNS);
        if (result != 0 && errno == EPERM)
            GTEST_SKIP() << "mount namespace requires CAP_SYS_ADMIN";
        ASSERT_EQ(0, result);
        ASSERT_EQ(0, mount(nullptr, "/", nullptr, MS_PRIVATE | MS_REC, nullptr));
        char pattern[] = "/tmp/dirent-metadata-XXXXXX";
        char* path = mkdtemp(pattern);
        ASSERT_NE(nullptr, path);
        base_ = path;
        ASSERT_EQ(0, mount("none", base_.c_str(), "ramfs", 0, nullptr));
        mounted_ = true;
    }
    void TearDown() override {
        if (mounted_) {
            while (umount2(base_.c_str(), MNT_DETACH) == 0) {}
        }
        if (!base_.empty()) rmdir(base_.c_str());
    }
    std::string base_;
    bool mounted_ = false;
};
TEST_F(DirectoryEntryMetadata, MountedChildReportsUnderlyingInode) {
    const std::string child = base_ + "/child";
    ASSERT_EQ(0, mkdir(child.c_str(), 0700));
    struct stat lower{}, upper{};
    ASSERT_EQ(0, stat(child.c_str(), &lower));
    ASSERT_EQ(0, mount("none", child.c_str(), "ramfs", 0, nullptr));
    ASSERT_EQ(0, stat(child.c_str(), &upper));
    ASSERT_NE(lower.st_ino, upper.st_ino) << "fixture needs distinct inode numbers";
    Fd dir(open(base_.c_str(), O_RDONLY | O_DIRECTORY));
    ASSERT_GE(dir.get(), 0);
    Entries entries;
    ASSERT_TRUE(ReadRest(dir.get(), &entries));
    ASSERT_EQ(1u, entries.count("child"));
    EXPECT_EQ(static_cast<uint64_t>(lower.st_ino), entries.at("child").ino);
    EXPECT_NE(static_cast<uint64_t>(upper.st_ino), entries.at("child").ino);
    EXPECT_EQ(4, entries.at("child").type);  // DT_DIR
}
TEST_F(DirectoryEntryMetadata, RetainedDirectoryUsesItsOriginalChildrenAfterOvermount) {
    const std::string item = base_ + "/item";
    ASSERT_EQ(0, symlink("target", item.c_str()));
    struct stat lower{};
    ASSERT_EQ(0, lstat(item.c_str(), &lower));
    Fd retained(open(base_.c_str(), O_RDONLY | O_DIRECTORY));
    ASSERT_GE(retained.get(), 0);
    ASSERT_EQ(0, mount("none", base_.c_str(), "ramfs", 0, nullptr));
    ASSERT_EQ(0, mkdir(item.c_str(), 0700));
    Entries entries;
    ASSERT_TRUE(ReadRest(retained.get(), &entries));
    ASSERT_EQ(1u, entries.count("item"));
    EXPECT_EQ(static_cast<uint64_t>(lower.st_ino), entries.at("item").ino);
    EXPECT_EQ(10, entries.at("item").type);  // DT_LNK, not upper DT_DIR
    // Remove the top mount; TearDown retires the original ramfs.
    ASSERT_EQ(0, umount2(base_.c_str(), MNT_DETACH));
}
TEST(DirectoryEntryMetadataProc, SmallBatchesValidateClosedAndReusedSlots) {
    Fd source(open("/dev/null", O_RDONLY));
    ASSERT_GE(source.get(), 0);
    Fd dir(open("/proc/self/fd", O_RDONLY | O_DIRECTORY));
    ASSERT_GE(dir.get(), 0);
    int watched = fcntl(source.get(), F_DUPFD_CLOEXEC, 200);
    ASSERT_GE(watched, 200);
    const std::string name = std::to_string(watched);
    Entries first;
    bool eof;
    ASSERT_TRUE(ReadBatch(dir.get(), &first, &eof));
    ASSERT_FALSE(eof);
    ASSERT_EQ(0u, first.count(name));  // mutation must precede this slot's emission
    ASSERT_EQ(0, close(watched));
    ASSERT_TRUE(ReadRest(dir.get(), &first, 24));
    EXPECT_EQ(0u, first.count(name));
    Entries at_eof;
    ASSERT_TRUE(ReadBatch(dir.get(), &at_eof, &eof));
    EXPECT_TRUE(eof);
    EXPECT_TRUE(at_eof.empty());

    ASSERT_EQ(watched, dup2(source.get(), watched));
    Fd restored(watched);
    ASSERT_EQ(0, lseek(dir.get(), 0, SEEK_SET));
    Entries second;
    ASSERT_TRUE(ReadBatch(dir.get(), &second, &eof));
    ASSERT_FALSE(eof);
    ASSERT_EQ(0u, second.count(name));
    Fd replacement(open("/dev/zero", O_RDONLY));
    ASSERT_GE(replacement.get(), 0);
    ASSERT_EQ(watched, dup2(replacement.get(), watched));
    struct stat link_stat{};
    const std::string link = "/proc/self/fd/" + name;
    ASSERT_EQ(0, lstat(link.c_str(), &link_stat));
    ASSERT_TRUE(ReadRest(dir.get(), &second, 24));
    ASSERT_EQ(1u, second.count(name));
    EXPECT_EQ(static_cast<uint64_t>(link_stat.st_ino), second.at(name).ino);
    EXPECT_EQ(10, second.at(name).type);
    char target[128];
    ssize_t n = readlink(link.c_str(), target, sizeof(target));
    ASSERT_GE(n, 0);
    EXPECT_EQ("/dev/zero", std::string(target, n));
    ASSERT_EQ(0, lseek(dir.get(), 0, SEEK_SET));
    Entries third;
    ASSERT_TRUE(ReadRest(dir.get(), &third, 24));
    ASSERT_EQ(1u, third.count(name));
    EXPECT_EQ(second.at(name).ino, third.at(name).ino);
}
}  // namespace
int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
