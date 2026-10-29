#include <gtest/gtest.h>
#include "loop_ext4_test_support.h"
#include <dirent.h>
#include <sys/statfs.h>
#include <array>
#include <cctype>
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

    uint64_t RootOffset() const {
        if (geometry_.slots != 0) return geometry_.root;
        const auto le16 = [&](unsigned n) {
            return uint32_t(boot_[n]) | (uint32_t(boot_[n + 1]) << 8);
        };
        const auto le32 = [&](unsigned n) {
            return le16(n) | (le16(n + 2) << 16);
        };
        const uint64_t first_data = le16(14) + uint64_t(boot_[16]) * le32(36);
        return (first_data + uint64_t(le32(44) - 2) * boot_[13]) * le16(11);
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

TEST_P(FatDirectory, ImportedShortAttributesDoNotConsumeFollowingEntries) {
    // Reuse one genuinely allocated directory chain; raw regular files below
    // are empty with cluster zero, so the imported image has no cross-links.
    ASSERT_NO_FATAL_FAILURE(Mount());
    ASSERT_EQ(0, mkdir(Path("template").c_str(), 0700)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(loop_.Unmount());
    const uint64_t root = RootOffset();
    ScopedFd raw {open(loop_.loop_path().c_str(), O_RDWR)};
    ASSERT_GE(raw.value, 0) << strerror(errno);
    std::array<unsigned char, 512> existing {};
    ASSERT_EQ(512, pread(raw.value, existing.data(), existing.size(), root));
    std::array<unsigned char, 32> directory {};
    bool found_directory = false;
    for (size_t offset = 0; offset != existing.size(); offset += 32) {
        if (existing[offset + 11] == 0x10) {
            memcpy(directory.data(), existing.data() + offset, directory.size());
            found_directory = true;
            break;
        }
    }
    ASSERT_TRUE(found_directory);
    std::array<unsigned char, 512> imported {};
    auto slot = [&](unsigned index, const char* name, unsigned char attribute) {
        unsigned char* entry = imported.data() + index * 32;
        memcpy(entry, name, 11);
        entry[11] = attribute;
        // Canonical valid 1980-01-01 dates; this test isolates attribute parsing.
        for (unsigned position : {16U, 18U, 24U}) entry[position] = 33;
    };
    slot(0, "IMPORT     ", 0x08); // Volume label is not a visible filename.
    memcpy(imported.data() + 32, directory.data(), directory.size());
    memcpy(imported.data() + 32, "SUBDIR     ", 11);
    imported[32 + 12] = 0; // Imported SFN is intentionally uppercase.
    slot(2, "PLAIN   TXT", 0x20);
    slot(3, "READONLYTXT", 0x21);
    slot(4, "HIDDEN  TXT", 0x22);
    slot(5, "SYSTEM  TXT", 0x24);
    slot(6, "SPECIAL TXT", 0x27);
    slot(7, "AFTER   TXT", 0x20);
    ASSERT_EQ(512, pwrite(raw.value, imported.data(), imported.size(), root));
    ASSERT_EQ(0, fsync(raw.value)) << strerror(errno);
    ASSERT_EQ(0, close(raw.value));
    raw.value = -1;
    ASSERT_NO_FATAL_FAILURE(Mount());
    for (int pass = 0; pass != 2; ++pass) {
        std::set<std::string> names;
        for (std::string name : Names()) {
            for (char& value : name) value = std::tolower(static_cast<unsigned char>(value));
            names.insert(name);
        }
        EXPECT_EQ(0U, names.count("import"));
        EXPECT_EQ(1U, names.count("subdir"));
        struct stat status {};
        ASSERT_EQ(0, stat(Path("SUBDIR").c_str(), &status)) << strerror(errno);
        EXPECT_TRUE(S_ISDIR(status.st_mode));
        for (const char* name : {"plain.txt", "readonly.txt", "hidden.txt",
                                  "system.txt", "special.txt", "after.txt"}) {
            EXPECT_EQ(1U, names.count(name)) << name;
            ASSERT_EQ(0, stat(Path(name).c_str(), &status)) << name << ": " << strerror(errno);
            EXPECT_TRUE(S_ISREG(status.st_mode)) << name;
            EXPECT_EQ(0, status.st_size) << name;
            ScopedFd readable {open(Path(name).c_str(), O_RDONLY)};
            ASSERT_GE(readable.value, 0) << name << ": " << strerror(errno);
            char value = 1;
            EXPECT_EQ(0, read(readable.value, &value, 1)) << name;
        }
        if (pass == 0) {
            ASSERT_NO_FATAL_FAILURE(Remount());
        }
    }
    ASSERT_NO_FATAL_FAILURE(Create("appended"));
    ASSERT_NO_FATAL_FAILURE(loop_.Unmount());
    ScopedFd inspect {open(loop_.loop_path().c_str(), O_RDONLY)};
    ASSERT_GE(inspect.value, 0) << strerror(errno);
    std::array<unsigned char, 32> label {};
    ASSERT_EQ(32, pread(inspect.value, label.data(), label.size(), root));
    EXPECT_EQ(0, memcmp(label.data(), imported.data(), label.size()))
        << "a volume label is occupied metadata, not a reusable directory slot";
}

// Import disk entries rather than creating names through the implementation
// under test: either backend may choose to create an LFN for a lowercase name.
TEST_P(FatDirectory, ImportedShortNameCaseBitsDisplayWithoutChangingDiskNames) {
    ASSERT_NO_FATAL_FAILURE(Mount());
    ASSERT_EQ(0, mkdir(Path("template").c_str(), 0700)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(loop_.Unmount());
    const uint64_t root = RootOffset();
    ScopedFd raw {open(loop_.loop_path().c_str(), O_RDWR)};
    ASSERT_GE(raw.value, 0) << strerror(errno);
    std::array<unsigned char, 512> existing {};
    ASSERT_EQ(512, pread(raw.value, existing.data(), existing.size(), root));
    std::array<unsigned char, 32> directory {};
    bool found_directory = false;
    for (size_t offset = 0; offset != existing.size(); offset += 32) {
        if (existing[offset + 11] == 0x10) {
            memcpy(directory.data(), existing.data() + offset, directory.size());
            found_directory = true;
            break;
        }
    }
    ASSERT_TRUE(found_directory);
    struct ImportedName {
        const char* raw;
        unsigned char bits;
        const char* display;
        const char* alternate;
    };
    const ImportedName cases[] = {
        {"UPPER   TXT", 0x00, "UPPER.TXT", "upper.txt"},
        {"BASELOW TXT", 0x08, "baselow.TXT", "BASELOW.txt"},
        {"EXTLOW  TXT", 0x10, "EXTLOW.txt", "extlow.TXT"},
        {"BOTHLOW TXT", 0x18, "bothlow.txt", "BOTHLOW.TXT"},
        {"NOEXT      ", 0x08, "noext", "NOEXT"},
        {"DIG1-3  A1 ", 0x18, "dig1-3.a1", "DIG1-3.A1"},
        {"RESERVE TXT", 0xE7, "RESERVE.TXT", "reserve.txt"},
        {"RESLOW  TXT", 0xFF, "reslow.txt", "RESLOW.TXT"},
        {"CHILD      ", 0x08, "child", "CHILD"},
    };
    std::array<unsigned char, 512> imported {};
    for (size_t index = 0; index != std::size(cases); ++index) {
        auto* entry = imported.data() + index * 32;
        if (index + 1 == std::size(cases)) {
            // Reuse a real allocated directory, including its . and .. entries.
            memcpy(entry, directory.data(), directory.size());
        } else {
            entry[11] = 0x20;
            for (unsigned position : {16U, 18U, 24U}) entry[position] = 33;
        }
        memcpy(entry, cases[index].raw, 11);
        entry[12] = cases[index].bits;
    }
    ASSERT_EQ(512, pwrite(raw.value, imported.data(), imported.size(), root));
    ASSERT_EQ(0, fsync(raw.value));
    ASSERT_EQ(0, close(raw.value));
    raw.value = -1;
    ASSERT_NO_FATAL_FAILURE(Mount());
    for (unsigned pass = 0; pass != 2; ++pass) {
        const auto names = Names();
        for (size_t index = 0; index != std::size(cases); ++index) {
            const auto& item = cases[index];
            SCOPED_TRACE(item.display);
            EXPECT_EQ(1U, names.count(item.display));
            EXPECT_EQ(0U, names.count(item.alternate));
            struct stat canonical {}, alternate {};
            ASSERT_EQ(0, stat(Path(item.display).c_str(), &canonical)) << strerror(errno);
            ASSERT_EQ(0, stat(Path(item.alternate).c_str(), &alternate)) << strerror(errno);
            EXPECT_EQ(canonical.st_ino, alternate.st_ino);
            EXPECT_EQ(canonical.st_dev, alternate.st_dev);
            if (index + 1 == std::size(cases)) {
                EXPECT_TRUE(S_ISDIR(canonical.st_mode));
                EXPECT_EQ(1U, Names(item.display).count("."));
                EXPECT_EQ(1U, Names(item.display).count(".."));
            } else {
                EXPECT_TRUE(S_ISREG(canonical.st_mode));
                ScopedFd readable {open(Path(item.alternate).c_str(), O_RDONLY)};
                ASSERT_GE(readable.value, 0) << strerror(errno);
                char byte = 1;
                EXPECT_EQ(0, read(readable.value, &byte, 1));
            }
        }
        if (pass == 0) {
            ASSERT_NO_FATAL_FAILURE(Remount());
        }
    }
    ASSERT_NO_FATAL_FAILURE(loop_.Unmount());
    raw.value = open(loop_.loop_path().c_str(), O_RDONLY);
    ASSERT_GE(raw.value, 0);
    std::array<unsigned char, 512> after {};
    ASSERT_EQ(512, pread(raw.value, after.data(), after.size(), root));
    for (size_t index = 0; index != std::size(cases); ++index) {
        EXPECT_EQ(0, memcmp(after.data() + index * 32, cases[index].raw, 11));
        EXPECT_EQ(cases[index].bits, after[index * 32 + 12]);
    }
}

TEST_P(FatDirectory, ShortNameCaseBitsPreserveLongNamesAndRenameTargetEncoding) {
    const uint64_t root = RootOffset();
    std::array<unsigned char, 512> imported {};
    const auto short_entry = [&](unsigned index, const char* name) {
        auto* entry = imported.data() + index * 32;
        memcpy(entry, name, 11);
        entry[11] = 0x20;
        entry[12] = 0x18;
        for (unsigned position : {16U, 18U, 24U}) entry[position] = 33;
    };
    short_entry(0, "TARGET  TXT");
    short_entry(2, "MIXED   TXT");
    auto* lfn = imported.data() + 32;
    lfn[0] = 0x41;
    lfn[11] = 0x0F;
    unsigned char checksum = 0;
    for (unsigned index = 0; index != 11; ++index) {
        checksum = static_cast<unsigned char>(((checksum & 1) << 7)
            + (checksum >> 1) + imported[64 + index]);
    }
    lfn[13] = checksum;
    const std::string mixed = "MiXeD.TxT";
    unsigned character = 0;
    for (unsigned offset : {1U, 3U, 5U, 7U, 9U, 14U, 16U, 18U, 20U, 22U, 24U, 28U, 30U}) {
        const unsigned value = character < mixed.size()
            ? static_cast<unsigned char>(mixed[character])
            : character == mixed.size() ? 0 : 0xFFFF;
        lfn[offset] = value & 0xFF;
        lfn[offset + 1] = value >> 8;
        ++character;
    }
    ScopedFd raw {open(loop_.loop_path().c_str(), O_RDWR)};
    ASSERT_GE(raw.value, 0);
    ASSERT_EQ(512, pwrite(raw.value, imported.data(), imported.size(), root));
    ASSERT_EQ(0, fsync(raw.value));
    ASSERT_EQ(0, close(raw.value));
    raw.value = -1;
    ASSERT_NO_FATAL_FAILURE(Mount());
    ASSERT_NO_FATAL_FAILURE(Create("payload-source.txt"));
    // Match an imported target through a case alias. Its spelling belongs to
    // the destination entry, including the original SFN case bits.
    ASSERT_EQ(0, rename(Path("payload-source.txt").c_str(), Path("TaRgEt.TxT").c_str()))
        << strerror(errno);
    for (unsigned pass = 0; pass != 2; ++pass) {
        const auto names = Names();
        EXPECT_EQ(1U, names.count("target.txt"));
        EXPECT_EQ(0U, names.count("TARGET.TXT"));
        EXPECT_EQ(1U, names.count(mixed));
        EXPECT_EQ(0U, names.count("mixed.txt"));
        EXPECT_EQ(0U, names.count("payload-source.txt"));
        ASSERT_NO_FATAL_FAILURE(Read("TARGET.TXT"));
        struct stat original {}, alias {};
        ASSERT_EQ(0, stat(Path(mixed).c_str(), &original));
        ASSERT_EQ(0, stat(Path("mixed.txt").c_str(), &alias));
        EXPECT_EQ(original.st_ino, alias.st_ino);
        if (pass == 0) {
            ASSERT_NO_FATAL_FAILURE(Remount());
        }
    }
    ASSERT_NO_FATAL_FAILURE(loop_.Unmount());
    raw.value = open(loop_.loop_path().c_str(), O_RDONLY);
    ASSERT_GE(raw.value, 0);
    std::array<unsigned char, 512> after {};
    ASSERT_EQ(512, pread(raw.value, after.data(), after.size(), root));
    for (unsigned index : {0U, 2U}) {
        EXPECT_EQ(0, memcmp(after.data() + index * 32, imported.data() + index * 32, 11));
        EXPECT_EQ(0x18, after[index * 32 + 12]);
    }
    EXPECT_EQ(0, memcmp(after.data() + 32, imported.data() + 32, 32))
        << "SFN case flags must not rewrite the LFN or its checksum";
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
