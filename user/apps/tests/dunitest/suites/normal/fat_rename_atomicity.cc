#include <gtest/gtest.h>
#include "loop_ext4_test_support.h"

#include <array>
#include <cstdio>
#include <string>
#include <sys/stat.h>

namespace {

struct ScopedFd {
    int value = -1;
    ~ScopedFd() { if (value >= 0) close(value); }
    int Close() {
        const int fd = value;
        value = -1;
        return close(fd);
    }
};

class FatRenameAtomicity : public testing::Test {
protected:
    void SetUp() override {
        ASSERT_NO_FATAL_FAILURE(fs_.SetUp("fat_stat_blocks.img", "/tmp", "fat_rename_atomicity"));
        ASSERT_NO_FATAL_FAILURE(fs_.Mount(0, "vfat"));
    }

    std::string Path(const std::string& name) const { return fs_.mount_point() + "/" + name; }

    void Create(const std::string& name, const char* contents) {
        ScopedFd fd {open(Path(name).c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)};
        ASSERT_GE(fd.value, 0) << name << ": " << strerror(errno);
        const ssize_t length = static_cast<ssize_t>(strlen(contents));
        EXPECT_EQ(length, write(fd.value, contents, length)) << strerror(errno);
        EXPECT_EQ(0, fsync(fd.value)) << strerror(errno);
        ASSERT_EQ(0, fd.Close()) << strerror(errno);
    }

    void ExpectContents(const std::string& name, const char* expected) {
        ScopedFd fd {open(Path(name).c_str(), O_RDONLY)};
        ASSERT_GE(fd.value, 0) << name << ": " << strerror(errno);
        std::array<char, 64> data {};
        ssize_t count = read(fd.value, data.data(), data.size());
        EXPECT_EQ(static_cast<ssize_t>(strlen(expected)), count);
        if (count >= 0) {
            EXPECT_EQ(std::string(expected), std::string(data.data(), count));
        }
        ASSERT_EQ(0, fd.Close()) << strerror(errno);
    }

    void FillRoot() {
        // This fixture has a fixed FAT16 root. Use single-slot names and do
        // not infer ENOSPC from an arbitrary number of successful creates.
        bool full = false;
        for (unsigned i = 0; i != 256; ++i) {
            char name[9];
            snprintf(name, sizeof(name), "F%07u", i);
            ScopedFd fd {open(Path(name).c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)};
            if (fd.value < 0) {
                ASSERT_EQ(ENOSPC, errno);
                full = true;
                break;
            }
            ASSERT_EQ(0, fd.Close());
        }
        ASSERT_TRUE(full) << "fixture must exhaust the fixed root";
    }

    void Remount() {
        ASSERT_NO_FATAL_FAILURE(fs_.Unmount());
        ASSERT_NO_FATAL_FAILURE(fs_.Mount(0, "vfat"));
    }

    void RootNameCase(const char* short_name, unsigned bits, bool write_bits) {
        ASSERT_NO_FATAL_FAILURE(fs_.Unmount());
        ScopedFd fd {open(fs_.backing_path().c_str(), O_RDWR)};
        ASSERT_GE(fd.value, 0);
        std::array<unsigned char, 512> boot {};
        ASSERT_EQ(512, pread(fd.value, boot.data(), boot.size(), 0));
        const auto le16 = [](const unsigned char* bytes) {
            return static_cast<unsigned>(bytes[0]) | (static_cast<unsigned>(bytes[1]) << 8);
        };
        const uint64_t root = (le16(&boot[14]) + boot[16] * le16(&boot[22])) * le16(&boot[11]);
        std::array<unsigned char, 11> name {};
        name.fill(' ');
        ASSERT_LE(strlen(short_name), 8U);
        memcpy(name.data(), short_name, strlen(short_name));
        std::array<unsigned char, 32> slot {};
        bool found = false;
        for (unsigned i = 0; i != le16(&boot[17]); ++i) {
            const uint64_t offset = root + i * 32;
            ASSERT_EQ(32, pread(fd.value, slot.data(), slot.size(), offset));
            if (memcmp(slot.data(), name.data(), name.size()) != 0) continue;
            found = true;
            if (write_bits) {
                slot[12] = bits;
                ASSERT_EQ(32, pwrite(fd.value, slot.data(), slot.size(), offset));
                ASSERT_EQ(0, fsync(fd.value));
            } else {
                EXPECT_EQ(bits, slot[12]);
            }
            break;
        }
        EXPECT_TRUE(found) << short_name;
        ASSERT_EQ(0, fd.Close());
        // Raw backing edits must invalidate the loop device block cache.
        if (write_bits) { ASSERT_NO_FATAL_FAILURE(fs_.Rebind()); }
        ASSERT_NO_FATAL_FAILURE(fs_.Mount(0, "vfat"));
    }

    void ExpectDiskParent() {
        // Verify the FAT on-disk '..' contract, which VFS pathname '..'
        // resolution may satisfy from its own dentry parent cache.
        ASSERT_NO_FATAL_FAILURE(fs_.Unmount());
        ScopedFd fd {open(fs_.backing_path().c_str(), O_RDONLY)};
        ASSERT_GE(fd.value, 0);
        std::array<unsigned char, 512> boot {};
        ASSERT_EQ(512, pread(fd.value, boot.data(), boot.size(), 0));
        auto le16 = [](const unsigned char* bytes) {
            return static_cast<unsigned>(bytes[0]) | (static_cast<unsigned>(bytes[1]) << 8);
        };
        const unsigned sector_size = le16(&boot[11]);
        const unsigned root_slots = le16(&boot[17]);
        const uint64_t root = (le16(&boot[14]) + boot[16] * le16(&boot[22])) * sector_size;
        const uint64_t data = root + ((root_slots * 32 + sector_size - 1) / sector_size) * sector_size;
        const uint64_t cluster_size = boot[13] * sector_size;
        std::array<unsigned char, 32> entry {};
        unsigned right_cluster = 0;
        for (unsigned i = 0; i != root_slots; ++i) {
            ASSERT_EQ(32, pread(fd.value, entry.data(), entry.size(), root + i * 32));
            if (memcmp(entry.data(), "RIGHT      ", 11) == 0) {
                right_cluster = le16(&entry[26]);
                break;
            }
        }
        ASSERT_GE(right_cluster, 2U);
        const uint64_t right_offset = data + (right_cluster - 2) * cluster_size;
        unsigned moved_cluster = 0;
        for (unsigned i = 0; i != cluster_size / 32; ++i) {
            ASSERT_EQ(32, pread(fd.value, entry.data(), entry.size(), right_offset + i * 32));
            if (memcmp(entry.data(), "MOVED      ", 11) == 0) {
                moved_cluster = le16(&entry[26]);
                break;
            }
        }
        ASSERT_GE(moved_cluster, 2U);
        ASSERT_EQ(32, pread(fd.value, entry.data(), entry.size(), data + (moved_cluster - 2) * cluster_size + 32));
        EXPECT_EQ(0, memcmp(entry.data(), "..         ", 11));
        EXPECT_EQ(right_cluster, le16(&entry[26]));
        ASSERT_EQ(0, fd.Close());
        ASSERT_NO_FATAL_FAILURE(fs_.Mount(0, "vfat"));
    }

    dunitest::LoopExt4 fs_;
};

TEST_F(FatRenameAtomicity, SameDirectoryNoSpacePreservesSourceAndOldDescriptor) {
    ASSERT_NO_FATAL_FAILURE(Create("SOURCE", "source data"));
    ScopedFd source {open(Path("SOURCE").c_str(), O_RDWR)};
    ASSERT_GE(source.value, 0);
    struct stat before {};
    ASSERT_EQ(0, fstat(source.value, &before));
    ASSERT_NO_FATAL_FAILURE(FillRoot());
    const std::string target(255, 'x');
    errno = 0;
    EXPECT_EQ(-1, rename(Path("SOURCE").c_str(), Path(target).c_str()));
    EXPECT_EQ(ENOSPC, errno);
    struct stat after {};
    EXPECT_EQ(0, stat(Path("SOURCE").c_str(), &after)) << strerror(errno);
    EXPECT_EQ(before.st_ino, after.st_ino);
    EXPECT_EQ(before.st_nlink, after.st_nlink);
    const timespec times[2] = {{1600000000, 0}, {1600000000, 0}};
    EXPECT_EQ(0, futimens(source.value, times));
    EXPECT_EQ(0, fsync(source.value));
    EXPECT_EQ(0, source.Close());
    ASSERT_NO_FATAL_FAILURE(ExpectContents("SOURCE", "source data"));
    ASSERT_NO_FATAL_FAILURE(Remount());
    ASSERT_NO_FATAL_FAILURE(ExpectContents("SOURCE", "source data"));
    EXPECT_EQ(-1, stat(Path(target).c_str(), &after));
    EXPECT_EQ(ENOENT, errno);
}

TEST_F(FatRenameAtomicity, CrossDirectoryNoSpacePreservesSource) {
    ASSERT_EQ(0, mkdir(Path("DIR").c_str(), 0700));
    ASSERT_NO_FATAL_FAILURE(Create("DIR/SOURCE", "cross data"));
    ASSERT_NO_FATAL_FAILURE(FillRoot());
    const std::string target(255, 'x');
    errno = 0;
    EXPECT_EQ(-1, rename(Path("DIR/SOURCE").c_str(), Path(target).c_str()));
    EXPECT_EQ(ENOSPC, errno);
    ASSERT_NO_FATAL_FAILURE(ExpectContents("DIR/SOURCE", "cross data"));
    ASSERT_NO_FATAL_FAILURE(Remount());
    ASSERT_NO_FATAL_FAILURE(ExpectContents("DIR/SOURCE", "cross data"));
}

TEST_F(FatRenameAtomicity, FullRootReplacementReusesTargetAndRetainsVictimDescriptor) {
    ASSERT_NO_FATAL_FAILURE(Create("SOURCE", "source data"));
    ASSERT_NO_FATAL_FAILURE(Create("TARGET", "victim data"));
    ScopedFd victim {open(Path("TARGET").c_str(), O_RDWR)};
    ASSERT_GE(victim.value, 0);
    ASSERT_NO_FATAL_FAILURE(FillRoot());
    EXPECT_EQ(0, rename(Path("SOURCE").c_str(), Path("TARGET").c_str())) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(ExpectContents("TARGET", "source data"));
    char data[32] {};
    EXPECT_EQ(11, pread(victim.value, data, sizeof(data), 0));
    EXPECT_EQ(std::string("victim data"), std::string(data, 11));
    const timespec times[2] = {{1600000000, 0}, {1600000000, 0}};
    EXPECT_EQ(0, futimens(victim.value, times));
    EXPECT_EQ(0, fsync(victim.value));
    EXPECT_EQ(0, victim.Close());
    ASSERT_NO_FATAL_FAILURE(Remount());
    ASSERT_NO_FATAL_FAILURE(ExpectContents("TARGET", "source data"));
    struct stat status {};
    EXPECT_EQ(-1, stat(Path("SOURCE").c_str(), &status));
    EXPECT_EQ(ENOENT, errno);
}

TEST_F(FatRenameAtomicity, InvalidTargetAndNonemptyVictimPreserveBothNames) {
    ASSERT_NO_FATAL_FAILURE(Create("SOURCE", "source data"));
    ASSERT_EQ(0, mkdir(Path("DIR").c_str(), 0700));
    ASSERT_EQ(0, mkdir(Path("VICTIM").c_str(), 0700));
    ASSERT_NO_FATAL_FAILURE(Create("VICTIM/FILE", "victim data"));
    errno = 0;
    EXPECT_EQ(-1, rename(Path("SOURCE").c_str(), Path(std::string(256, 'x')).c_str()));
    EXPECT_EQ(ENAMETOOLONG, errno);
    errno = 0;
    EXPECT_EQ(-1, rename(Path("DIR").c_str(), Path("VICTIM").c_str()));
    EXPECT_EQ(ENOTEMPTY, errno);
    ASSERT_NO_FATAL_FAILURE(ExpectContents("SOURCE", "source data"));
    ASSERT_NO_FATAL_FAILURE(ExpectContents("VICTIM/FILE", "victim data"));
    ASSERT_NO_FATAL_FAILURE(Remount());
    ASSERT_NO_FATAL_FAILURE(ExpectContents("SOURCE", "source data"));
    ASSERT_NO_FATAL_FAILURE(ExpectContents("VICTIM/FILE", "victim data"));
}

TEST_F(FatRenameAtomicity, CrossDirectoryReplacementKeepsVictimDataOnOldDescriptor) {
    ASSERT_EQ(0, mkdir(Path("LEFT").c_str(), 0700));
    ASSERT_EQ(0, mkdir(Path("RIGHT").c_str(), 0700));
    ASSERT_NO_FATAL_FAILURE(Create("LEFT/source-long-file-name", "source data"));
    ASSERT_NO_FATAL_FAILURE(Create("RIGHT/target-long-file-name", "victim data"));
    ScopedFd victim {open(Path("RIGHT/target-long-file-name").c_str(), O_RDWR)};
    ASSERT_GE(victim.value, 0);
    ASSERT_EQ(0, rename(Path("LEFT/source-long-file-name").c_str(),
                        Path("RIGHT/target-long-file-name").c_str())) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(ExpectContents("RIGHT/target-long-file-name", "source data"));
    char data[32] {};
    EXPECT_EQ(11, pread(victim.value, data, sizeof(data), 0));
    EXPECT_EQ(std::string("victim data"), std::string(data, 11));
    struct stat victim_status {};
    EXPECT_EQ(0, fstat(victim.value, &victim_status));
    EXPECT_EQ(0U, victim_status.st_nlink);
    EXPECT_EQ(0, victim.Close());
    ASSERT_NO_FATAL_FAILURE(Remount());
    ASSERT_NO_FATAL_FAILURE(ExpectContents("RIGHT/target-long-file-name", "source data"));
    struct stat status {};
    EXPECT_EQ(-1, stat(Path("LEFT/source-long-file-name").c_str(), &status));
    EXPECT_EQ(ENOENT, errno);
}

TEST_F(FatRenameAtomicity, ImportedSfnCaseBelongsToDestinationName) {
    ASSERT_NO_FATAL_FAILURE(Create("SOURCE", "source data"));
    ASSERT_NO_FATAL_FAILURE(Create("TARGET", "victim data"));
    // Import a legal lowercase SFN display without an LFN. Replacement keeps
    // the target's name metadata, rather than importing the source's case.
    ASSERT_NO_FATAL_FAILURE(RootNameCase("TARGET", 0x08, true));
    ASSERT_EQ(0, rename(Path("SOURCE").c_str(), Path("target").c_str()));
    ASSERT_NO_FATAL_FAILURE(RootNameCase("TARGET", 0x08, false));
    ASSERT_NO_FATAL_FAILURE(ExpectContents("target", "source data"));
    ASSERT_NO_FATAL_FAILURE(Create("SOURCE", "new source"));
    ASSERT_NO_FATAL_FAILURE(RootNameCase("SOURCE", 0x08, true));
    ASSERT_EQ(0, rename(Path("source").c_str(), Path("NEWNAME").c_str()));
    // A fresh uppercase destination must not inherit the old source's bits.
    ASSERT_NO_FATAL_FAILURE(RootNameCase("NEWNAME", 0, false));
    ASSERT_NO_FATAL_FAILURE(ExpectContents("NEWNAME", "new source"));
}

TEST_F(FatRenameAtomicity, DirectoryMoveKeepsContentsAndNewParentAcrossRemount) {
    ASSERT_EQ(0, mkdir(Path("LEFT").c_str(), 0700));
    ASSERT_EQ(0, mkdir(Path("RIGHT").c_str(), 0700));
    ASSERT_EQ(0, mkdir(Path("LEFT/CHILD").c_str(), 0700));
    ASSERT_NO_FATAL_FAILURE(Create("LEFT/CHILD/FILE", "nested data"));
    ASSERT_EQ(0, rename(Path("LEFT/CHILD").c_str(), Path("RIGHT/MOVED").c_str())) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(ExpectContents("RIGHT/MOVED/FILE", "nested data"));
    struct stat right {}, parent {};
    ASSERT_EQ(0, stat(Path("RIGHT").c_str(), &right));
    ASSERT_EQ(0, stat(Path("RIGHT/MOVED/..").c_str(), &parent));
    EXPECT_EQ(right.st_ino, parent.st_ino);
    ASSERT_NO_FATAL_FAILURE(ExpectDiskParent());
    ASSERT_NO_FATAL_FAILURE(ExpectContents("RIGHT/MOVED/FILE", "nested data"));
    ASSERT_EQ(0, stat(Path("RIGHT").c_str(), &right));
    ASSERT_EQ(0, stat(Path("RIGHT/MOVED/..").c_str(), &parent));
    EXPECT_EQ(right.st_ino, parent.st_ino);
}

TEST(FatRenameRootParent, Fat32RootParentUsesZeroForCreateAndMove) {
    dunitest::LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp("fat32_stat_blocks.img", "/tmp", "fat32_rename_parent"));
    ASSERT_NO_FATAL_FAILURE(fs.Mount(0, "vfat"));
    const auto path = [&](const char* name) { return fs.mount_point() + "/" + name; };
    ASSERT_EQ(0, mkdir(path("PARENT").c_str(), 0700));
    ASSERT_EQ(0, mkdir(path("PARENT/CHILD").c_str(), 0700));
    ASSERT_EQ(0, rename(path("PARENT/CHILD").c_str(), path("MOVED").c_str()));
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
    ScopedFd fd {open(fs.backing_path().c_str(), O_RDONLY)};
    ASSERT_GE(fd.value, 0);
    std::array<unsigned char, 512> boot {};
    ASSERT_EQ(512, pread(fd.value, boot.data(), boot.size(), 0));
    const auto le16 = [](const unsigned char* bytes) {
        return static_cast<unsigned>(bytes[0]) | (static_cast<unsigned>(bytes[1]) << 8);
    };
    const auto le32 = [](const unsigned char* bytes) {
        return static_cast<uint64_t>(bytes[0]) | (static_cast<uint64_t>(bytes[1]) << 8)
            | (static_cast<uint64_t>(bytes[2]) << 16) | (static_cast<uint64_t>(bytes[3]) << 24);
    };
    ASSERT_EQ(0U, le16(&boot[17]));
    const uint64_t sector_size = le16(&boot[11]);
    const uint64_t cluster_size = sector_size * boot[13];
    const uint64_t data = (le16(&boot[14]) + boot[16] * le32(&boot[36])) * sector_size;
    const uint64_t root = data + (le32(&boot[44]) - 2) * cluster_size;
    std::array<unsigned char, 32> entry {};
    unsigned checked = 0;
    for (unsigned i = 0; i != cluster_size / 32; ++i) {
        ASSERT_EQ(32, pread(fd.value, entry.data(), entry.size(), root + i * 32));
        if (memcmp(entry.data(), "PARENT     ", 11) != 0 &&
            memcmp(entry.data(), "MOVED      ", 11) != 0) {
            continue;
        }
        const uint64_t cluster = le16(&entry[26]) | (static_cast<uint64_t>(le16(&entry[20])) << 16);
        ASSERT_GE(cluster, 2U);
        ASSERT_EQ(32, pread(fd.value, entry.data(), entry.size(), data + (cluster - 2) * cluster_size + 32));
        EXPECT_EQ(0, memcmp(entry.data(), "..         ", 11));
        EXPECT_EQ(0U, le16(&entry[26]));
        EXPECT_EQ(0U, le16(&entry[20]));
        ++checked;
    }
    EXPECT_EQ(2U, checked);
    ASSERT_EQ(0, fd.Close());
    ASSERT_NO_FATAL_FAILURE(fs.Mount(0, "vfat"));
    ASSERT_EQ(0, rmdir(path("MOVED").c_str()));
    ASSERT_EQ(0, rmdir(path("PARENT").c_str()));
}

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
