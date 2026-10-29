#include <gtest/gtest.h>
#include "loop_ext4_test_support.h"

#include <algorithm>
#include <array>
#include <cerrno>
#include <cstdlib>
#include <cstring>
#include <fcntl.h>
#include <linux/stat.h>
#include <string>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

namespace {

constexpr time_t kOldTime = 1600000001;
constexpr time_t kRequestedTime = 1700000001;

struct ScopedFd {
    int value;
    ~ScopedFd() { if (value >= 0) close(value); }
    void Close() {
        const int fd = value;
        value = -1;
        ASSERT_EQ(0, close(fd)) << strerror(errno);
    }
};

class FatTimestamps : public testing::Test {
protected:
    void SetUp() override {
        ASSERT_NO_FATAL_FAILURE(loop_.SetUp("fat32_stat_blocks.img", "/tmp", "fat_timestamps"));
        ASSERT_NO_FATAL_FAILURE(loop_.Mount(0, "vfat"));
        struct timezone zone {};
        ASSERT_EQ(0, syscall(SYS_gettimeofday, nullptr, &zone)) << strerror(errno);
        minutes_west_ = zone.tz_minuteswest;
    }

    std::string Path(const char* name) const { return loop_.mount_point() + "/" + name; }

    void Create(const char* name, size_t length = 4096) {
        int fd = open(Path(name).c_str(), O_CREAT | O_EXCL | O_RDWR, 0600);
        ASSERT_GE(fd, 0) << strerror(errno);
        std::array<char, 4096> bytes {};
        for (size_t done = 0; done < length;) {
            const size_t count = std::min(length - done, bytes.size());
            const ssize_t written = write(fd, bytes.data(), count);
            if (written <= 0) {
                const int error = errno;
                close(fd);
                FAIL() << "create data: " << strerror(error);
            }
            done += static_cast<size_t>(written);
        }
        const int result = close(fd);
        ASSERT_EQ(0, result) << strerror(errno);
    }

    void SetTime(const char* name, time_t seconds) {
        const timespec times[2] = {{seconds, 123456789}, {seconds, 987654321}};
        ASSERT_EQ(0, utimensat(AT_FDCWD, Path(name).c_str(), times, 0)) << strerror(errno);
    }

    void Sync(const char* name, bool directory = false) {
        int fd = open(Path(name).c_str(), O_RDONLY | (directory ? O_DIRECTORY : 0));
        ASSERT_GE(fd, 0) << strerror(errno);
        const int result = fsync(fd);
        const int error = errno;
        EXPECT_EQ(0, close(fd)) << strerror(errno);
        ASSERT_EQ(0, result) << strerror(error);
    }

    void Remount() {
        ASSERT_NO_FATAL_FAILURE(loop_.Unmount());
        ASSERT_NO_FATAL_FAILURE(loop_.Mount(0, "vfat"));
    }

    void ExpectTime(const char* name, time_t seconds) {
        struct stat status {};
        ASSERT_EQ(0, stat(Path(name).c_str(), &status)) << strerror(errno);
        EXPECT_EQ(seconds & ~time_t(1), status.st_mtim.tv_sec);
        EXPECT_EQ(0, status.st_mtim.tv_nsec);
        EXPECT_EQ(status.st_mtim.tv_sec, status.st_ctim.tv_sec);
        EXPECT_EQ(0, status.st_ctim.tv_nsec);
        const time_t offset = minutes_west_ * 60;
        const time_t local = seconds - offset;
        const time_t midnight = (local / 86400) * 86400 + offset;
        EXPECT_EQ(midnight, status.st_atim.tv_sec);
        EXPECT_EQ(0, status.st_atim.tv_nsec);
    }

    void ExpectModified(const char* name, time_t before, time_t after, time_t* observed) {
        struct stat status {};
        ASSERT_EQ(0, stat(Path(name).c_str(), &status)) << strerror(errno);
        EXPECT_GE(status.st_mtim.tv_sec, before & ~time_t(1));
        EXPECT_LE(status.st_mtim.tv_sec, after & ~time_t(1));
        EXPECT_EQ(0, status.st_mtim.tv_nsec);
        EXPECT_EQ(status.st_mtim.tv_sec, status.st_ctim.tv_sec);
        EXPECT_EQ(0, status.st_ctim.tv_nsec);
        *observed = status.st_mtim.tv_sec;
    }

    void BirthTime(const char* name, struct statx_timestamp* result) {
        struct statx status {};
        ASSERT_EQ(0, syscall(SYS_statx, AT_FDCWD, Path(name).c_str(), 0,
                             STATX_BTIME, &status)) << strerror(errno);
        ASSERT_NE(0U, status.stx_mask & STATX_BTIME);
        ScopedFd fd {open(Path(name).c_str(), O_RDONLY | O_CLOEXEC)};
        ASSERT_GE(fd.value, 0) << strerror(errno);
        struct statx by_fd {};
        ASSERT_EQ(0, syscall(SYS_statx, fd.value, "", AT_EMPTY_PATH,
                             STATX_BTIME, &by_fd)) << strerror(errno);
        EXPECT_NE(0U, by_fd.stx_mask & STATX_BTIME);
        EXPECT_EQ(status.stx_btime.tv_sec, by_fd.stx_btime.tv_sec);
        EXPECT_EQ(status.stx_btime.tv_nsec, by_fd.stx_btime.tv_nsec);
        ASSERT_NO_FATAL_FAILURE(fd.Close());
        *result = status.stx_btime;
    }

    dunitest::LoopExt4 loop_;
    int minutes_west_ = 0;
};

TEST_F(FatTimestamps, ExplicitTimesSurviveRemountForFileAndDirectory) {
    ASSERT_NO_FATAL_FAILURE(Create("file"));
    ASSERT_EQ(0, mkdir(Path("directory").c_str(), 0700)) << strerror(errno);
    for (const char* name : {"file", "directory"}) {
        ASSERT_NO_FATAL_FAILURE(SetTime(name, kRequestedTime));
        ASSERT_NO_FATAL_FAILURE(ExpectTime(name, kRequestedTime));
        ASSERT_NO_FATAL_FAILURE(Sync(name, std::string(name) == "directory"));
    }
    ASSERT_NO_FATAL_FAILURE(Remount());
    ASSERT_NO_FATAL_FAILURE(ExpectTime("file", kRequestedTime));
    ASSERT_NO_FATAL_FAILURE(ExpectTime("directory", kRequestedTime));
}

TEST_F(FatTimestamps, CalendarBoundariesSurviveRemount) {
    ASSERT_NO_FATAL_FAILURE(Create("file"));
    // Local calendar dates: 1980-01-01, 2000-02-29, 2100-03-01,
    // and FAT's last representable day. No clock changes or sleeping.
    for (time_t local : {time_t(315532800), time_t(951782400),
                         time_t(4107542400), time_t(4354732800)}) {
        const time_t requested = local + minutes_west_ * 60 + 1;
        ASSERT_NO_FATAL_FAILURE(SetTime("file", requested));
        ASSERT_NO_FATAL_FAILURE(ExpectTime("file", requested));
        ASSERT_NO_FATAL_FAILURE(Sync("file"));
        ASSERT_NO_FATAL_FAILURE(Remount());
        ASSERT_NO_FATAL_FAILURE(ExpectTime("file", requested));
    }
}

TEST_F(FatTimestamps, OutOfRangeExplicitTimesClampBeforeAndAfterRemount) {
    ASSERT_NO_FATAL_FAILURE(Create("file"));
    const time_t offset = minutes_west_ * 60;
    // Linux notify_change clamps to s_time_min/max before fat_truncate_time.
    // Include negative time, which must not enter unsigned calendar arithmetic.
    for (time_t requested : {time_t(-1), time_t(5000000001)}) {
        const time_t expected = requested < 0 ? 315532800 + offset
                                             : 4354819198 + offset;
        ASSERT_NO_FATAL_FAILURE(SetTime("file", requested));
        ASSERT_NO_FATAL_FAILURE(ExpectTime("file", expected));
        ASSERT_NO_FATAL_FAILURE(Sync("file"));
        ASSERT_NO_FATAL_FAILURE(Remount());
        ASSERT_NO_FATAL_FAILURE(ExpectTime("file", expected));
    }
}

TEST_F(FatTimestamps, MetadataOnlySyncfsPersistsExplicitTimes) {
    ASSERT_NO_FATAL_FAILURE(Create("file"));
    ASSERT_NO_FATAL_FAILURE(Sync("file"));
    ASSERT_NO_FATAL_FAILURE(Remount());
    ASSERT_NO_FATAL_FAILURE(SetTime("file", kRequestedTime));
    ScopedFd fd {open(Path("file").c_str(), O_RDONLY)};
    ASSERT_GE(fd.value, 0) << strerror(errno);
    ASSERT_EQ(0, syncfs(fd.value)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(fd.Close());
    ASSERT_NO_FATAL_FAILURE(Remount());
    ASSERT_NO_FATAL_FAILURE(ExpectTime("file", kRequestedTime));
}

TEST_F(FatTimestamps, RootTimesStayZero) {
    struct statx_timestamp birth {};
    ASSERT_NO_FATAL_FAILURE(BirthTime("", &birth));
    EXPECT_EQ(0, birth.tv_sec);
    EXPECT_EQ(0U, birth.tv_nsec);
    const timespec times[2] = {{kRequestedTime, 123}, {kRequestedTime, 456}};
    ASSERT_EQ(0, utimensat(AT_FDCWD, loop_.mount_point().c_str(), times, 0))
        << strerror(errno);
    for (int pass = 0; pass != 2; ++pass) {
        struct stat status {};
        ASSERT_EQ(0, stat(loop_.mount_point().c_str(), &status)) << strerror(errno);
        EXPECT_EQ(0, status.st_atim.tv_sec);
        EXPECT_EQ(0, status.st_mtim.tv_sec);
        EXPECT_EQ(0, status.st_ctim.tv_sec);
        EXPECT_EQ(0, status.st_atim.tv_nsec);
        EXPECT_EQ(0, status.st_mtim.tv_nsec);
        EXPECT_EQ(0, status.st_ctim.tv_nsec);
        if (pass == 0) {
            ASSERT_NO_FATAL_FAILURE(Remount());
        }
    }
}

TEST_F(FatTimestamps, BufferedAndDirectWritesPersistOperationTime) {
    for (bool direct : {false, true}) {
        const char* name = direct ? "direct" : "buffered";
        ASSERT_NO_FATAL_FAILURE(Create(name));
        ASSERT_NO_FATAL_FAILURE(SetTime(name, kOldTime));
        int fd = open(Path(name).c_str(), O_RDWR | (direct ? O_DIRECT : 0));
        ASSERT_GE(fd, 0) << strerror(errno);
        alignas(4096) std::array<char, 4096> bytes {};
        bytes.fill('w');
        const time_t before = time(nullptr);
        const ssize_t written = pwrite(fd, bytes.data(), bytes.size(), 0);
        const time_t after = time(nullptr);
        const int error = errno;
        const int synced = fsync(fd);
        EXPECT_EQ(0, close(fd)) << strerror(errno);
        ASSERT_EQ(static_cast<ssize_t>(bytes.size()), written) << strerror(error);
        ASSERT_EQ(0, synced) << strerror(errno);
        time_t observed = 0;
        ASSERT_NO_FATAL_FAILURE(ExpectModified(name, before, after, &observed));
        ASSERT_NO_FATAL_FAILURE(Remount());
        time_t restored = 0;
        ASSERT_NO_FATAL_FAILURE(ExpectModified(name, before, after, &restored));
        EXPECT_EQ(observed, restored);
    }
}

TEST_F(FatTimestamps, DelayedWritebackPreservesLaterExplicitTime) {
    ASSERT_NO_FATAL_FAILURE(Create("file"));
    int fd = open(Path("file").c_str(), O_RDWR);
    ASSERT_GE(fd, 0) << strerror(errno);
    const ssize_t written = pwrite(fd, "dirty", 5, 0);
    EXPECT_EQ(0, close(fd)) << strerror(errno);
    ASSERT_EQ(5, written);
    ASSERT_NO_FATAL_FAILURE(SetTime("file", kRequestedTime));
    ASSERT_NO_FATAL_FAILURE(Sync("file"));
    ASSERT_NO_FATAL_FAILURE(Remount());
    ASSERT_NO_FATAL_FAILURE(ExpectTime("file", kRequestedTime));
}

TEST_F(FatTimestamps, TruncateAndFallocatePersistTimesAndNoopKeepsTime) {
    ASSERT_NO_FATAL_FAILURE(Create("file"));
    for (int operation = 0; operation != 4; ++operation) {
        ASSERT_NO_FATAL_FAILURE(SetTime("file", kOldTime));
        int fd = open(Path("file").c_str(), O_RDWR);
        ASSERT_GE(fd, 0) << strerror(errno);
        const time_t before = time(nullptr);
        int result;
        if (operation < 2) {
            result = ftruncate(fd, operation == 0 ? 8192 : 2048);
        } else {
            result = fallocate(fd, 0, 0, operation == 2 ? 12288 : 1024);
        }
        const time_t after = time(nullptr);
        const int error = errno;
        const int synced = fsync(fd);
        EXPECT_EQ(0, close(fd)) << strerror(errno);
        ASSERT_EQ(0, result) << strerror(error);
        ASSERT_EQ(0, synced) << strerror(errno);
        time_t observed = 0;
        if (operation == 3) {
            ASSERT_NO_FATAL_FAILURE(ExpectTime("file", kOldTime));
        } else {
            ASSERT_NO_FATAL_FAILURE(ExpectModified("file", before, after, &observed));
        }
        ASSERT_NO_FATAL_FAILURE(Remount());
        if (operation == 3) {
            ASSERT_NO_FATAL_FAILURE(ExpectTime("file", kOldTime));
        } else {
            time_t restored = 0;
            ASSERT_NO_FATAL_FAILURE(ExpectModified("file", before, after, &restored));
            EXPECT_EQ(observed, restored);
        }
    }
}

TEST_F(FatTimestamps, RenamePreservesFileAndDirectoryBirthTime) {
    ASSERT_NO_FATAL_FAILURE(Create("file"));
    ASSERT_EQ(0, mkdir(Path("directory").c_str(), 0700)) << strerror(errno);
    for (const char* name : {"file", "directory"}) {
        struct statx_timestamp before {};
        ASSERT_NO_FATAL_FAILURE(BirthTime(name, &before));
        EXPECT_GT(before.tv_sec, kOldTime);
        EXPECT_EQ(0U, before.tv_nsec % 10000000);
        ASSERT_NO_FATAL_FAILURE(SetTime(name, kRequestedTime));
        const std::string destination = std::string(name) + "_renamed";
        ASSERT_EQ(0, rename(Path(name).c_str(), Path(destination.c_str()).c_str()))
            << strerror(errno);
        ASSERT_NO_FATAL_FAILURE(Sync(destination.c_str(), std::string(name) == "directory"));
        ASSERT_NO_FATAL_FAILURE(Remount());
        struct statx_timestamp after {};
        ASSERT_NO_FATAL_FAILURE(BirthTime(destination.c_str(), &after));
        EXPECT_EQ(before.tv_sec, after.tv_sec);
        EXPECT_EQ(before.tv_nsec, after.tv_nsec);
    }
}

TEST_F(FatTimestamps, UnlinkedOpenFileCannotOverwriteReusedDirectorySlot) {
    ASSERT_NO_FATAL_FAILURE(Create("file"));
    ScopedFd old {open(Path("file").c_str(), O_RDWR)};
    ASSERT_GE(old.value, 0) << strerror(errno);
    const int removed = unlink(Path("file").c_str());
    if (removed != 0) {
        const int error = errno;
        FAIL() << strerror(error);
    }
    ASSERT_NO_FATAL_FAILURE(Create("file"));
    ASSERT_NO_FATAL_FAILURE(SetTime("file", kRequestedTime));
    const timespec times[2] = {{kOldTime, 0}, {kOldTime, 0}};
    EXPECT_EQ(0, futimens(old.value, times)) << strerror(errno);
    EXPECT_EQ(3, pwrite(old.value, "old", 3, 0)) << strerror(errno);
    EXPECT_EQ(0, fsync(old.value)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(old.Close());
    ASSERT_NO_FATAL_FAILURE(Sync("file"));
    ASSERT_NO_FATAL_FAILURE(Remount());
    ASSERT_NO_FATAL_FAILURE(ExpectTime("file", kRequestedTime));
    struct stat status {};
    ASSERT_EQ(0, stat(Path("file").c_str(), &status)) << strerror(errno);
    EXPECT_EQ(4096, status.st_size);
    int fd = open(Path("file").c_str(), O_RDONLY);
    ASSERT_GE(fd, 0) << strerror(errno);
    char value = 1;
    EXPECT_EQ(1, pread(fd, &value, 1, 0));
    EXPECT_EQ(0, value);
    EXPECT_EQ(0, close(fd));
}

TEST_F(FatTimestamps, UnlinkAndRmdirUpdateOnlyRemovedDescriptorTimes) {
    ASSERT_NO_FATAL_FAILURE(Create("file"));
    ASSERT_EQ(0, mkdir(Path("directory").c_str(), 0700)) << strerror(errno);
    for (bool directory : {false, true}) {
        const char* name = directory ? "directory" : "file";
        ASSERT_NO_FATAL_FAILURE(SetTime(name, kOldTime));
        ScopedFd fd {open(Path(name).c_str(), O_RDONLY | (directory ? O_DIRECTORY : 0))};
        ASSERT_GE(fd.value, 0) << strerror(errno);
        const time_t before = time(nullptr);
        ASSERT_EQ(0, directory ? rmdir(Path(name).c_str()) : unlink(Path(name).c_str()))
            << strerror(errno);
        const time_t after = time(nullptr);
        struct stat status {};
        ASSERT_EQ(0, fstat(fd.value, &status)) << strerror(errno);
        EXPECT_EQ(0U, status.st_nlink);
        EXPECT_GE(status.st_mtim.tv_sec, before & ~time_t(1));
        EXPECT_LE(status.st_mtim.tv_sec, after & ~time_t(1));
        EXPECT_EQ(status.st_mtim.tv_sec, status.st_ctim.tv_sec);
        EXPECT_EQ(0, status.st_mtim.tv_nsec);
        EXPECT_EQ(0, status.st_ctim.tv_nsec);
        const time_t offset = minutes_west_ * 60;
        EXPECT_GE(status.st_atim.tv_sec, ((before - offset) / 86400) * 86400 + offset);
        EXPECT_LE(status.st_atim.tv_sec, ((after - offset) / 86400) * 86400 + offset);
        EXPECT_EQ(0, status.st_atim.tv_nsec);
    }
}

TEST_F(FatTimestamps, RemovedDirectoryRetainsBlocksUntilLastDescriptorCloses) {
    struct statfs before {};
    ASSERT_EQ(0, statfs(loop_.mount_point().c_str(), &before)) << strerror(errno);
    ASSERT_EQ(0, mkdir(Path("directory").c_str(), 0700)) << strerror(errno);
    ScopedFd old {open(Path("directory").c_str(), O_RDONLY | O_DIRECTORY)};
    ASSERT_GE(old.value, 0) << strerror(errno);
    struct stat allocated {};
    ASSERT_EQ(0, fstat(old.value, &allocated)) << strerror(errno);
    ASSERT_GT(allocated.st_blocks, 0);
    struct statfs live {};
    ASSERT_EQ(0, statfs(loop_.mount_point().c_str(), &live)) << strerror(errno);
    ASSERT_LT(live.f_bfree, before.f_bfree);
    ASSERT_EQ(0, rmdir(Path("directory").c_str())) << strerror(errno);
    struct stat removed {};
    ASSERT_EQ(0, fstat(old.value, &removed)) << strerror(errno);
    EXPECT_EQ(0U, removed.st_nlink);
    EXPECT_EQ(allocated.st_blocks, removed.st_blocks);
    struct statfs retained {};
    ASSERT_EQ(0, statfs(loop_.mount_point().c_str(), &retained)) << strerror(errno);
    EXPECT_EQ(live.f_bfree, retained.f_bfree);
    ASSERT_NO_FATAL_FAILURE(old.Close());
    ScopedFd root {open(loop_.mount_point().c_str(), O_RDONLY | O_DIRECTORY)};
    ASSERT_GE(root.value, 0) << strerror(errno);
    ASSERT_EQ(0, syncfs(root.value)) << strerror(errno);
    struct statfs reclaimed {};
    ASSERT_EQ(0, statfs(loop_.mount_point().c_str(), &reclaimed)) << strerror(errno);
    EXPECT_EQ(before.f_bfree, reclaimed.f_bfree);
}

TEST_F(FatTimestamps, NamespaceChangesPersistParentModificationTime) {
    ASSERT_EQ(0, mkdir(Path("parent").c_str(), 0700)) << strerror(errno);
    for (int operation = 0; operation != 5; ++operation) {
        ASSERT_NO_FATAL_FAILURE(SetTime("parent", kOldTime));
        const time_t before = time(nullptr);
        if (operation == 0) {
            ASSERT_NO_FATAL_FAILURE(Create("parent/file"));
        } else if (operation == 1) {
            ASSERT_EQ(0, mkdir(Path("parent/child").c_str(), 0700)) << strerror(errno);
        } else if (operation == 2) {
            ASSERT_EQ(0, rename(Path("parent/file").c_str(),
                                Path("parent/renamed").c_str())) << strerror(errno);
        } else if (operation == 3) {
            ASSERT_EQ(0, unlink(Path("parent/renamed").c_str())) << strerror(errno);
        } else {
            ASSERT_EQ(0, rmdir(Path("parent/child").c_str())) << strerror(errno);
        }
        const time_t after = time(nullptr);
        time_t observed = 0;
        ASSERT_NO_FATAL_FAILURE(ExpectModified("parent", before, after, &observed));
        ASSERT_NO_FATAL_FAILURE(Sync("parent", true));
        ASSERT_NO_FATAL_FAILURE(Remount());
        time_t restored = 0;
        ASSERT_NO_FATAL_FAILURE(ExpectModified("parent", before, after, &restored));
        EXPECT_EQ(observed, restored);
    }
}

TEST_F(FatTimestamps, CrossDirectoryRenamePersistsBothParentTimes) {
    ASSERT_EQ(0, mkdir(Path("from").c_str(), 0700)) << strerror(errno);
    ASSERT_EQ(0, mkdir(Path("to").c_str(), 0700)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(Create("from/file"));
    ASSERT_NO_FATAL_FAILURE(SetTime("from", kOldTime));
    ASSERT_NO_FATAL_FAILURE(SetTime("to", kOldTime));
    const time_t before = time(nullptr);
    ASSERT_EQ(0, rename(Path("from/file").c_str(), Path("to/file").c_str()))
        << strerror(errno);
    const time_t after = time(nullptr);
    for (const char* name : {"from", "to"}) {
        time_t observed = 0;
        ASSERT_NO_FATAL_FAILURE(ExpectModified(name, before, after, &observed));
        ASSERT_NO_FATAL_FAILURE(Sync(name, true));
    }
    ASSERT_NO_FATAL_FAILURE(Remount());
    for (const char* name : {"from", "to"}) {
        time_t observed = 0;
        ASSERT_NO_FATAL_FAILURE(ExpectModified(name, before, after, &observed));
    }
}

TEST_F(FatTimestamps, ReplacedOpenDirectoryCannotOverwriteRenamedSlot) {
    ASSERT_EQ(0, mkdir(Path("source").c_str(), 0700)) << strerror(errno);
    ASSERT_EQ(0, mkdir(Path("target").c_str(), 0700)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(SetTime("target", kOldTime));
    ScopedFd old {open(Path("target").c_str(), O_RDONLY | O_DIRECTORY)};
    ASSERT_GE(old.value, 0) << strerror(errno);
    ASSERT_EQ(0, rename(Path("source").c_str(), Path("target").c_str()))
        << strerror(errno);
    struct stat replaced {};
    ASSERT_EQ(0, fstat(old.value, &replaced)) << strerror(errno);
    // Rename replacement asks for CTIME only, which FAT intentionally ignores.
    EXPECT_EQ(kOldTime & ~time_t(1), replaced.st_mtim.tv_sec);
    EXPECT_EQ(replaced.st_mtim.tv_sec, replaced.st_ctim.tv_sec);
    ASSERT_NO_FATAL_FAILURE(SetTime("target", kRequestedTime));
    const timespec times[2] = {{kOldTime, 0}, {kOldTime, 0}};
    EXPECT_EQ(0, futimens(old.value, times)) << strerror(errno);
    EXPECT_EQ(0, fsync(old.value)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(old.Close());
    ASSERT_NO_FATAL_FAILURE(Sync("target", true));
    ASSERT_NO_FATAL_FAILURE(Remount());
    ASSERT_NO_FATAL_FAILURE(ExpectTime("target", kRequestedTime));
}

TEST_F(FatTimestamps, RemovedOpenDirectoryCannotOverwriteReusedSlot) {
    ASSERT_EQ(0, mkdir(Path("directory").c_str(), 0700)) << strerror(errno);
    ScopedFd old {open(Path("directory").c_str(), O_RDONLY | O_DIRECTORY)};
    ASSERT_GE(old.value, 0) << strerror(errno);
    const int removed = rmdir(Path("directory").c_str());
    if (removed != 0) {
        const int error = errno;
        FAIL() << strerror(error);
    }
    ASSERT_EQ(0, mkdir(Path("directory").c_str(), 0700)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(SetTime("directory", kRequestedTime));
    const timespec times[2] = {{kOldTime, 0}, {kOldTime, 0}};
    EXPECT_EQ(0, futimens(old.value, times)) << strerror(errno);
    EXPECT_EQ(0, fsync(old.value)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(old.Close());
    ASSERT_NO_FATAL_FAILURE(Sync("directory", true));
    ASSERT_NO_FATAL_FAILURE(Remount());
    ASSERT_NO_FATAL_FAILURE(ExpectTime("directory", kRequestedTime));
}

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
