#include <gtest/gtest.h>
#include "loop_ext4_test_support.h"
#include <dirent.h>
#include <sys/statfs.h>
#include <array>
#include <cstdio>
#include <set>
#include <vector>

namespace {
struct Geometry {
    uint64_t root = 0;
    unsigned slots = 0;
};
struct ScopedFd {
    int value;
    ~ScopedFd() { if (value >= 0) close(value); }
};

class FatDirectory : public ::testing::TestWithParam<const char*> {
protected:
    void SetUp() override {
        ASSERT_NO_FATAL_FAILURE(loop_.SetUp(GetParam()));
        int fd = open(loop_.loop_path().c_str(), O_RDONLY);
        ASSERT_GE(fd, 0);
        std::array<unsigned char, 512> boot {};
        ASSERT_EQ(512, pread(fd, boot.data(), boot.size(), 0));
        boot_ = boot;
        ASSERT_EQ(0, close(fd));
        auto le16 = [&](unsigned n) { return boot[n] | (boot[n + 1] << 8); };
        geometry_.slots = le16(17);
        geometry_.root = static_cast<uint64_t>(le16(14) + boot[16] * le16(22)) * le16(11);
        ASSERT_EQ(512, le16(11));
    }

    void Mount() { ASSERT_NO_FATAL_FAILURE(loop_.Mount(0, "vfat")); }
    std::string Path(const std::string& name) { return loop_.mount_point() + "/" + name; }
    void Create(const std::string& name, bool payload = true) {
        ScopedFd opened {open(Path(name).c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)};
        int fd = opened.value;
        ASSERT_GE(fd, 0) << name << ": " << strerror(errno);
        if (payload) {
            ASSERT_EQ(7, write(fd, "payload", 7));
            ASSERT_EQ(0, fsync(fd)) << name << ": " << strerror(errno);
        }
        ASSERT_EQ(0, close(fd));
        opened.value = -1;
    }
    void Read(const std::string& name) {
        ScopedFd opened {open(Path(name).c_str(), O_RDONLY)};
        int fd = opened.value;
        ASSERT_GE(fd, 0) << name << ": " << strerror(errno);
        char data[8] {};
        EXPECT_EQ(7, read(fd, data, 7));
        EXPECT_STREQ("payload", data);
        EXPECT_EQ(0, close(fd));
        opened.value = -1;
    }
    std::set<std::string> Names(const std::string& directory = "") {
        std::set<std::string> names;
        DIR* dir = opendir(Path(directory).c_str());
        if (!dir) { ADD_FAILURE() << strerror(errno); return names; }
        errno = 0;
        while (auto* entry = readdir(dir)) names.insert(entry->d_name);
        EXPECT_EQ(0, errno);
        EXPECT_EQ(0, closedir(dir));
        return names;
    }
    void Remount() {
        ASSERT_NO_FATAL_FAILURE(loop_.Unmount());
        ASSERT_NO_FATAL_FAILURE(Mount());
    }
    // Legal pre-existing SFNs place the next LFN precisely at a sector boundary.
    void SeedSlots(unsigned count) {
        int fd = open(loop_.loop_path().c_str(), O_RDWR);
        ASSERT_GE(fd, 0);
        for (unsigned i = 0; i < count; ++i) {
            std::array<unsigned char, 32> slot {};
            char name[9];
            snprintf(name, sizeof(name), "F%07u", i % 10000000);
            memcpy(slot.data(), name, 8);
            memset(slot.data() + 8, ' ', 3);
            slot[11] = 0x20;
            ASSERT_EQ(32, pwrite(fd, slot.data(), slot.size(), geometry_.root + i * 32));
        }
        ASSERT_EQ(0, fsync(fd));
        ASSERT_EQ(0, close(fd));
    }
    dunitest::LoopExt4 loop_;
    Geometry geometry_;
    std::array<unsigned char, 512> boot_ {};
};

TEST_P(FatDirectory, CreateLookupAndRemount) {
    ASSERT_NO_FATAL_FAILURE(Mount());
    ASSERT_NO_FATAL_FAILURE(Create("blocks"));
    ASSERT_NO_FATAL_FAILURE(Create("a-long-directory-entry-name.txt"));
    EXPECT_EQ(1u, Names().count("blocks"));
    ASSERT_NO_FATAL_FAILURE(Remount());
    ASSERT_NO_FATAL_FAILURE(Read("blocks"));
    ASSERT_NO_FATAL_FAILURE(Read("a-long-directory-entry-name.txt"));
    errno = 0;
    EXPECT_EQ(-1, open(Path("missing").c_str(), O_RDONLY));
    EXPECT_EQ(ENOENT, errno);
    ASSERT_NO_FATAL_FAILURE(loop_.Unmount());
    int raw = open(loop_.loop_path().c_str(), O_RDONLY);
    ASSERT_GE(raw, 0);
    std::array<unsigned char, 512> after {};
    ASSERT_EQ(512, pread(raw, after.data(), after.size(), 0));
    ASSERT_EQ(0, close(raw));
    EXPECT_EQ(boot_, after) << "allocation must never overwrite the volume header";
}

TEST_P(FatDirectory, FixedRootCapacityAndReuse) {
    if (!geometry_.slots) {
        ASSERT_NO_FATAL_FAILURE(Mount());
        for (unsigned i = 0; i < 40; ++i)
            ASSERT_NO_FATAL_FAILURE(Create("root-growth-long-entry-" + std::to_string(i)));
        ASSERT_NO_FATAL_FAILURE(Remount());
        for (unsigned i = 0; i < 40; ++i)
            ASSERT_NO_FATAL_FAILURE(Read("root-growth-long-entry-" + std::to_string(i)));
        return;
    }
    std::array<unsigned char, 512> adjacent_before {}, adjacent_after {};
    int raw = open(loop_.loop_path().c_str(), O_RDONLY);
    ASSERT_GE(raw, 0);
    ASSERT_EQ(512, pread(raw, adjacent_before.data(), adjacent_before.size(),
                         geometry_.root + geometry_.slots * 32));
    ASSERT_EQ(0, close(raw));
    ASSERT_NO_FATAL_FAILURE(Mount());
    std::vector<std::string> names;
    for (unsigned i = 0; i <= geometry_.slots; ++i) {
        std::string name = "long-capacity-entry-" + std::to_string(i);
        int fd = open(Path(name).c_str(), O_CREAT | O_EXCL | O_RDWR, 0600);
        if (fd < 0) { ASSERT_EQ(ENOSPC, errno); break; }
        ASSERT_EQ(0, close(fd));
        names.push_back(name);
    }
    ASSERT_FALSE(names.empty());
    ASSERT_LT(names.size(), geometry_.slots);
    const auto before = Names();
    struct statfs space_before {}, space_after {};
    ASSERT_EQ(0, statfs(loop_.mount_point().c_str(), &space_before));
    for (int i = 0; i < 8; ++i) {
        errno = 0;
        EXPECT_EQ(-1, mkdir(Path("long-capacity-directory-overflow").c_str(), 0700));
        EXPECT_EQ(ENOSPC, errno);
    }
    ASSERT_EQ(0, statfs(loop_.mount_point().c_str(), &space_after));
    EXPECT_EQ(space_before.f_bfree, space_after.f_bfree);
    errno = 0;
    EXPECT_EQ(-1, open(Path(std::string(255, 'z')).c_str(), O_CREAT | O_EXCL | O_RDWR, 0600));
    EXPECT_EQ(ENOSPC, errno);
    EXPECT_EQ(before, Names());
    ASSERT_NO_FATAL_FAILURE(Remount());
    EXPECT_EQ(before, Names());
    ASSERT_EQ(0, unlink(Path(names.front()).c_str()));
    ASSERT_NO_FATAL_FAILURE(Create(names.front(), false));
    ASSERT_NO_FATAL_FAILURE(Remount());
    EXPECT_EQ(before, Names());
    ASSERT_NO_FATAL_FAILURE(loop_.Unmount());
    raw = open(loop_.loop_path().c_str(), O_RDONLY);
    ASSERT_GE(raw, 0);
    ASSERT_EQ(512, pread(raw, adjacent_after.data(), adjacent_after.size(),
                         geometry_.root + geometry_.slots * 32));
    ASSERT_EQ(0, close(raw));
    EXPECT_EQ(adjacent_before, adjacent_after) << "fixed root overflow corrupted adjacent data";
}

TEST_P(FatDirectory, FixedRootLastSlotsAndSectorCrossing) {
    if (!geometry_.slots) {
        ASSERT_NO_FATAL_FAILURE(Mount());
        ASSERT_NO_FATAL_FAILURE(Create(std::string(255, 'r')));
        ASSERT_NO_FATAL_FAILURE(Remount());
        ASSERT_NO_FATAL_FAILURE(Read(std::string(255, 'r')));
        ASSERT_EQ(0, unlink(Path(std::string(255, 'r')).c_str()));
        ASSERT_NO_FATAL_FAILURE(Remount());
        EXPECT_EQ(0u, Names().count(std::string(255, 'r')));
        return;
    }
    ASSERT_GE(geometry_.slots, 32u);
    ASSERT_NO_FATAL_FAILURE(SeedSlots(15));
    ASSERT_NO_FATAL_FAILURE(Mount());
    ASSERT_NO_FATAL_FAILURE(Create("cross-sector-long-name.txt"));
    ASSERT_NO_FATAL_FAILURE(Remount());
    ASSERT_NO_FATAL_FAILURE(Read("cross-sector-long-name.txt"));
    ASSERT_EQ(0, unlink(Path("cross-sector-long-name.txt").c_str()));
    ASSERT_NO_FATAL_FAILURE(loop_.Unmount());
    ASSERT_NO_FATAL_FAILURE(SeedSlots(geometry_.slots - 2));
    ASSERT_NO_FATAL_FAILURE(Mount());
    // 13 characters require one LFN and its SFN: ends at the last legal slot.
    ASSERT_NO_FATAL_FAILURE(Create("last-slot.txt", false));
    errno = 0;
    EXPECT_EQ(-1, open(Path("overflow").c_str(), O_CREAT | O_EXCL | O_RDWR, 0600));
    EXPECT_EQ(ENOSPC, errno);
    ASSERT_NO_FATAL_FAILURE(Remount());
    EXPECT_EQ(1u, Names().count("last-slot.txt"));
}

TEST_P(FatDirectory, SubdirectoryGrowthAndMaximumNameDeletion) {
    ASSERT_NO_FATAL_FAILURE(Mount());
    ASSERT_EQ(0, mkdir(Path("sub").c_str(), 0700));
    for (unsigned i = 0; i < 24; ++i)
        ASSERT_NO_FATAL_FAILURE(Create("sub/cross-cluster-entry-" + std::to_string(i)));
    const std::string maximum = "sub/" + std::string(255, 'm');
    ASSERT_NO_FATAL_FAILURE(Create(maximum));
    ASSERT_NO_FATAL_FAILURE(Remount());
    ASSERT_NO_FATAL_FAILURE(Read(maximum));
    ASSERT_EQ(0, unlink(Path(maximum).c_str()));
    ASSERT_NO_FATAL_FAILURE(Remount());
    EXPECT_EQ(0u, Names("sub").count(std::string(255, 'm')));
    struct stat directory_before {}, directory_after {};
    ASSERT_EQ(0, stat(Path("sub").c_str(), &directory_before));
    for (int i = 0; i < 6; ++i) {
        ASSERT_NO_FATAL_FAILURE(Create(maximum));
        ASSERT_NO_FATAL_FAILURE(Remount());
        ASSERT_NO_FATAL_FAILURE(Read(maximum));
        ASSERT_EQ(0, stat(Path("sub").c_str(), &directory_after));
        EXPECT_EQ(directory_before.st_blocks, directory_after.st_blocks)
            << "deletion must release every LFN/SFN slot for reuse";
        ASSERT_EQ(0, unlink(Path(maximum).c_str()));
        ASSERT_NO_FATAL_FAILURE(Remount());
    }
    for (unsigned i = 0; i < 24; ++i)
        ASSERT_NO_FATAL_FAILURE(Read("sub/cross-cluster-entry-" + std::to_string(i)));
}

TEST_P(FatDirectory, RenameAcrossDirectoriesAndRemove) {
    ASSERT_NO_FATAL_FAILURE(Mount());
    ASSERT_EQ(0, mkdir(Path("sub").c_str(), 0700));
    ASSERT_NO_FATAL_FAILURE(Create("long-original-name.txt"));
    ASSERT_EQ(0, rename(Path("long-original-name.txt").c_str(), Path("sub/renamed-long-name.txt").c_str()));
    ASSERT_NO_FATAL_FAILURE(Remount());
    ASSERT_NO_FATAL_FAILURE(Read("sub/renamed-long-name.txt"));
    ASSERT_EQ(0, unlink(Path("sub/renamed-long-name.txt").c_str()));
    ASSERT_EQ(0, rmdir(Path("sub").c_str()));
    ASSERT_NO_FATAL_FAILURE(Remount());
    EXPECT_EQ(0u, Names().count("long-original-name.txt"));
    EXPECT_EQ(0u, Names().count("sub"));
}

INSTANTIATE_TEST_SUITE_P(Formats, FatDirectory,
    ::testing::Values("fat12_directory.img", "fat16_directory.img", "fat32_stat_blocks.img"));

class FixedFatDirectory : public FatDirectory {};
TEST_P(FixedFatDirectory, OneTailSlotCannotPublishTwoSlotName) {
    ASSERT_GT(geometry_.slots, 0u);
    ASSERT_NO_FATAL_FAILURE(SeedSlots(geometry_.slots - 1));
    ASSERT_NO_FATAL_FAILURE(Mount());
    errno = 0;
    EXPECT_EQ(-1, open(Path("needs-lfn.txt").c_str(), O_CREAT | O_EXCL | O_RDWR, 0600));
    EXPECT_EQ(ENOSPC, errno);
    ASSERT_NO_FATAL_FAILURE(loop_.Unmount());
    // Neither the final free root slot nor the first data slot may be changed.
    int fd = open(loop_.loop_path().c_str(), O_RDONLY);
    ASSERT_GE(fd, 0);
    std::array<unsigned char, 64> boundary {};
    ASSERT_EQ(64, pread(fd, boundary.data(), boundary.size(),
                        geometry_.root + (geometry_.slots - 1) * 32));
    ASSERT_EQ(0, close(fd));
    const std::array<unsigned char, 64> zeros {};
    EXPECT_EQ(zeros, boundary);
    ASSERT_NO_FATAL_FAILURE(Mount());
    EXPECT_EQ(0u, Names().count("needs-lfn.txt"));
}
INSTANTIATE_TEST_SUITE_P(FixedFormats, FixedFatDirectory,
    ::testing::Values("fat12_directory.img", "fat16_directory.img"));
} // namespace
int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
