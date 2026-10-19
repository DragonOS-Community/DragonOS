#include <gtest/gtest.h>
#include "loop_ext4_test_support.h"

#include <array>
#include <csignal>
#include <cstdio>
#include <string>
#include <sys/stat.h>
#include <sys/wait.h>

namespace {

struct ScopedFd {
    int value = -1;
    ~ScopedFd() { if (value >= 0) close(value); }
};

class FatRenameAliases : public testing::Test {
protected:
    void SetUp() override {
        ASSERT_NO_FATAL_FAILURE(fs_.SetUp("fat_stat_blocks.img", "/tmp", "fat_rename_aliases"));
        ASSERT_NO_FATAL_FAILURE(fs_.Mount(0, "vfat"));
    }

    std::string Path(const std::string& name) const { return fs_.mount_point() + "/" + name; }

    void Create(const std::string& name, const char* contents) {
        ScopedFd fd{open(Path(name).c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)};
        ASSERT_GE(fd.value, 0) << name << ": " << strerror(errno);
        ASSERT_EQ(static_cast<ssize_t>(strlen(contents)), write(fd.value, contents, strlen(contents)));
        ASSERT_EQ(0, fsync(fd.value)) << strerror(errno);
    }

    void ExpectContents(const std::string& name, const char* contents) {
        ScopedFd fd{open(Path(name).c_str(), O_RDONLY)};
        ASSERT_GE(fd.value, 0) << name << ": " << strerror(errno);
        std::array<char, 128> bytes {};
        const ssize_t count = read(fd.value, bytes.data(), bytes.size());
        ASSERT_GE(count, 0) << strerror(errno);
        EXPECT_EQ(std::string(contents), std::string(bytes.data(), count));
    }

    void Remount() {
        ASSERT_NO_FATAL_FAILURE(fs_.Unmount());
        ASSERT_NO_FATAL_FAILURE(fs_.Mount(0, "vfat"));
    }

    // Discover the actual backend-generated alias, without assuming either
    // Linux's or DragonOS's short-name collision suffix algorithm.
    void AliasFor(const std::string& long_name, std::string* alias) {
        ASSERT_NO_FATAL_FAILURE(fs_.Unmount());
        {
            ScopedFd fd{open(fs_.backing_path().c_str(), O_RDONLY)};
            ASSERT_GE(fd.value, 0);
            std::array<unsigned char, 512> boot {};
            ASSERT_EQ(512, pread(fd.value, boot.data(), boot.size(), 0));
            const auto le16 = [](const unsigned char* data) {
                return unsigned(data[0]) | (unsigned(data[1]) << 8);
            };
            const unsigned sector_size = le16(&boot[11]);
            const unsigned slots = le16(&boot[17]);
            ASSERT_GT(slots, 0U) << "fixture must have a fixed FAT16 root";
            const uint64_t root = (le16(&boot[14]) + boot[16] * le16(&boot[22])) * sector_size;
            std::string decoded;
            for (unsigned index = 0; index != slots; ++index) {
                std::array<unsigned char, 32> slot {};
                ASSERT_EQ(32, pread(fd.value, slot.data(), slot.size(), root + index * 32));
                if (slot[0] == 0) break;
                if (slot[0] == 0xE5) { decoded.clear(); continue; }
                if (slot[11] == 0x0F) {
                    const unsigned ordinal = slot[0] & 0x1F;
                    ASSERT_GE(ordinal, 1U);
                    ASSERT_LE(ordinal, 20U);
                    if (slot[0] & 0x40) decoded.assign(260, '\0');
                    ASSERT_EQ(260U, decoded.size());
                    unsigned character = (ordinal - 1) * 13;
                    for (unsigned offset : {1U, 3U, 5U, 7U, 9U, 14U, 16U, 18U, 20U, 22U, 24U, 28U, 30U}) {
                        const unsigned value = le16(&slot[offset]);
                        decoded[character++] = value == 0 || value == 0xFFFF ? '\0' : char(value);
                    }
                    continue;
                }
                if (std::string(decoded.c_str()) == long_name) {
                    std::string base(reinterpret_cast<const char*>(slot.data()), 8);
                    std::string extension(reinterpret_cast<const char*>(slot.data() + 8), 3);
                    while (!base.empty() && base.back() == ' ') base.pop_back();
                    while (!extension.empty() && extension.back() == ' ') extension.pop_back();
                    *alias = base + (extension.empty() ? "" : "." + extension);
                    break;
                }
                decoded.clear();
            }
            ASSERT_FALSE(alias->empty()) << long_name;
        }
        ASSERT_NO_FATAL_FAILURE(fs_.Mount(0, "vfat"));
    }

    void ExpectAbsent(const std::string& name) {
        struct stat metadata {};
        errno = 0;
        EXPECT_EQ(-1, stat(Path(name).c_str(), &metadata));
        EXPECT_EQ(ENOENT, errno);
    }

    dunitest::LoopExt4 fs_;
};

TEST_F(FatRenameAliases, LongNameToItsOwnShortAliasIsNoOp) {
    const std::string name = "source-with-a-long-name.txt";
    ASSERT_NO_FATAL_FAILURE(Create(name, "source data"));
    std::string alias;
    ASSERT_NO_FATAL_FAILURE(AliasFor(name, &alias));
    ScopedFd original{open(Path(name).c_str(), O_RDWR)};
    ASSERT_GE(original.value, 0);
    struct stat before {}, after {};
    ASSERT_EQ(0, fstat(original.value, &before));
    ASSERT_EQ(0, rename(Path(name).c_str(), Path(alias).c_str())) << strerror(errno);
    ASSERT_EQ(0, stat(Path(name).c_str(), &after)) << strerror(errno);
    EXPECT_EQ(before.st_ino, after.st_ino);
    EXPECT_EQ(before.st_nlink, after.st_nlink);
    ASSERT_NO_FATAL_FAILURE(ExpectContents(alias, "source data"));
    const timespec times[2] = {{1600000000, 0}, {1600000000, 0}};
    EXPECT_EQ(0, futimens(original.value, times));
    EXPECT_EQ(0, fsync(original.value));
    ASSERT_EQ(0, close(original.value)); original.value = -1;
    ASSERT_NO_FATAL_FAILURE(Remount());
    ASSERT_NO_FATAL_FAILURE(ExpectContents(name, "source data"));
    ASSERT_NO_FATAL_FAILURE(ExpectContents(alias, "source data"));
}

TEST_F(FatRenameAliases, AliasDescriptorFollowsRenameAndCannotRewriteReusedSlot) {
    const std::string name = "source-with-a-long-name.txt";
    const std::string target = "renamed-with-a-long-name.txt";
    ASSERT_NO_FATAL_FAILURE(Create(name, "source data"));
    std::string alias;
    ASSERT_NO_FATAL_FAILURE(AliasFor(name, &alias));
    ScopedFd alias_fd{open(Path(alias).c_str(), O_RDWR)};
    ASSERT_GE(alias_fd.value, 0) << alias;
    ASSERT_EQ(0, rename(Path(name).c_str(), Path(target).c_str())) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(Create(name, "replacement"));
    const timespec times[2] = {{1600000000, 0}, {1600000000, 0}};
    EXPECT_EQ(0, futimens(alias_fd.value, times));
    EXPECT_EQ(11, pwrite(alias_fd.value, "changed src", 11, 0));
    EXPECT_EQ(0, fsync(alias_fd.value));
    ASSERT_EQ(0, close(alias_fd.value)); alias_fd.value = -1;
    ASSERT_NO_FATAL_FAILURE(ExpectContents(target, "changed src"));
    ASSERT_NO_FATAL_FAILURE(ExpectContents(name, "replacement"));
    ASSERT_NO_FATAL_FAILURE(Remount());
    ASSERT_NO_FATAL_FAILURE(ExpectContents(target, "changed src"));
    ASSERT_NO_FATAL_FAILURE(ExpectContents(name, "replacement"));
}

TEST_F(FatRenameAliases, ReplacingSourceParentFailsWithoutLockingItTwice) {
    ASSERT_EQ(0, mkdir(Path("PARENT").c_str(), 0700));
    ASSERT_EQ(0, mkdir(Path("PARENT/CHILD").c_str(), 0700));
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        alarm(4);
        errno = 0;
        const int result = rename(Path("PARENT/CHILD").c_str(), Path("PARENT").c_str());
        _exit(result == -1 && errno == ENOTEMPTY ? 0 : 1);
    }
    int status = 0;
    bool finished = false;
    for (unsigned attempt = 0; attempt != 100; ++attempt) {
        const pid_t result = waitpid(child, &status, WNOHANG);
        if (result == child) { finished = true; break; }
        ASSERT_EQ(0, result) << strerror(errno);
        usleep(50 * 1000);
    }
    if (!finished) {
        // An uninterruptible kernel lock wait cannot be cleaned up by a C++
        // mount destructor. Exit this test process so the runner has a bounded
        // result instead of entering a second wait in umount/waitpid.
        kill(child, SIGKILL);
        fprintf(stderr, "FAT rename parent-owner lock watchdog expired\n");
        fflush(stderr);
        _exit(124);
    }
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    struct stat metadata {};
    ASSERT_EQ(0, stat(Path("PARENT/CHILD").c_str(), &metadata));
    ASSERT_TRUE(S_ISDIR(metadata.st_mode));
    ASSERT_NO_FATAL_FAILURE(Remount());
    EXPECT_EQ(0, stat(Path("PARENT/CHILD").c_str(), &metadata));
}

TEST_F(FatRenameAliases, ReplacingTargetThroughAliasPreservesItsLongNameOwner) {
    const std::string victim = "victim-with-a-long-name.txt";
    ASSERT_NO_FATAL_FAILURE(Create("SOURCE", "source data"));
    ASSERT_NO_FATAL_FAILURE(Create(victim, "victim data"));
    std::string alias;
    ASSERT_NO_FATAL_FAILURE(AliasFor(victim, &alias));
    ScopedFd old_victim{open(Path(victim).c_str(), O_RDWR)};
    ASSERT_GE(old_victim.value, 0);
    ASSERT_EQ(0, rename(Path("SOURCE").c_str(), Path(alias).c_str())) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(ExpectContents(victim, "source data"));
    ASSERT_NO_FATAL_FAILURE(ExpectContents(alias, "source data"));
    const timespec times[2] = {{1600000000, 0}, {1600000000, 0}};
    EXPECT_EQ(0, futimens(old_victim.value, times));
    EXPECT_EQ(0, fsync(old_victim.value));
    ASSERT_EQ(0, close(old_victim.value)); old_victim.value = -1;
    ASSERT_NO_FATAL_FAILURE(ExpectAbsent("SOURCE"));
    ASSERT_NO_FATAL_FAILURE(Remount());
    ASSERT_NO_FATAL_FAILURE(ExpectContents(victim, "source data"));
    ASSERT_NO_FATAL_FAILURE(ExpectContents(alias, "source data"));
    // Both names are now hot in the parent cache. Removing the alias must
    // revoke the same canonical entry, not leave the old long name alive.
    ASSERT_EQ(0, unlink(Path(alias).c_str())) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(ExpectAbsent(victim));
    ASSERT_NO_FATAL_FAILURE(ExpectAbsent(alias));
    ASSERT_NO_FATAL_FAILURE(Remount());
    ASSERT_NO_FATAL_FAILURE(ExpectAbsent(victim));
    ASSERT_NO_FATAL_FAILURE(ExpectAbsent(alias));
}

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
