#include <gtest/gtest.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/xattr.h>
#include <sys/stat.h>
#include <sys/inotify.h>
#include <sys/wait.h>
#include <unistd.h>
#include <array>
#include <string>

namespace {
constexpr const char* kName = "security.capability";
constexpr uint32_t kRev2 = 0x02000000;
constexpr uint32_t kRev3 = 0x03000000;

// xattr capability words are little endian, including on non-x86 targets.
void put_word(uint8_t* destination, uint32_t value) {
    for (unsigned i = 0; i < 4; ++i) destination[i] = value >> (8 * i);
}
uint32_t word(const uint8_t* value) {
    return uint32_t(value[0]) | uint32_t(value[1]) << 8 |
           uint32_t(value[2]) << 16 | uint32_t(value[3]) << 24;
}

class MountFilecapsTest : public ::testing::Test {
protected:
    void SetUp() override {
        // Like ext4_xattr, use the persistent root filesystem: DragonOS /tmp
        // is tmpfs, whose optional security-xattr handler is not implemented.
        char pattern[] = "/root/dunitest_mount_filecaps_XXXXXX";
        fd_ = mkstemp(pattern);
        ASSERT_GE(fd_, 0) << strerror(errno);
        path_ = pattern;
    }
    void TearDown() override {
        if (fd_ >= 0) close(fd_);
        if (!path_.empty()) unlink(path_.c_str());
    }
    std::array<uint8_t, 24> capability(uint32_t revision, uint32_t root = 0) {
        std::array<uint8_t, 24> result {};
        put_word(result.data(), revision | 1);
        put_word(result.data() + 4, 1); // CAP_CHOWN permitted, not exercised.
        put_word(result.data() + 20, root);
        return result;
    }
    void set(const std::array<uint8_t, 24>& value, size_t size) {
        ASSERT_EQ(0, fsetxattr(fd_, kName, value.data(), size, 0)) << strerror(errno);
    }
    int fd_ = -1;
    std::string path_;
};

TEST_F(MountFilecapsTest, RevisionTwoAndRootZeroRevisionThreeReadAsTwo) {
    auto value = capability(kRev2);
    set(value, 20);
    ASSERT_FALSE(HasFatalFailure());
    EXPECT_EQ(20, fgetxattr(fd_, kName, nullptr, 0));
    std::array<uint8_t, 24> read {};
    ASSERT_EQ(20, fgetxattr(fd_, kName, read.data(), read.size())) << strerror(errno);
    EXPECT_EQ(kRev2 | 1, word(read.data()));
    value = capability(kRev3, 0);
    set(value, 24);
    ASSERT_FALSE(HasFatalFailure());
    EXPECT_EQ(20, fgetxattr(fd_, kName, nullptr, 0));
    ASSERT_EQ(20, fgetxattr(fd_, kName, read.data(), read.size())) << strerror(errno);
    EXPECT_EQ(kRev2 | 1, word(read.data()));
}

TEST_F(MountFilecapsTest, NonzeroRootAndShortOutputPreserveNamespaceValue) {
    auto value = capability(kRev3, 1000);
    set(value, 24);
    ASSERT_FALSE(HasFatalFailure());
    EXPECT_EQ(24, fgetxattr(fd_, kName, nullptr, 0));
    std::array<uint8_t, 24> read {};
    EXPECT_EQ(-1, fgetxattr(fd_, kName, read.data(), 20));
    EXPECT_EQ(ERANGE, errno);
    ASSERT_EQ(24, fgetxattr(fd_, kName, read.data(), read.size())) << strerror(errno);
    EXPECT_EQ(kRev3 | 1, word(read.data()));
    EXPECT_EQ(1000U, word(read.data() + 20));
    ASSERT_EQ(0, fremovexattr(fd_, kName)) << strerror(errno);
    EXPECT_EQ(-1, fgetxattr(fd_, kName, nullptr, 0));
    EXPECT_EQ(ENODATA, errno);
}

TEST_F(MountFilecapsTest, InvalidHeaderDoesNotReplaceExistingAttribute) {
    auto valid = capability(kRev2);
    set(valid, 20);
    ASSERT_FALSE(HasFatalFailure());
    auto invalid = valid;
    put_word(invalid.data(), kRev2 | 2); // Only EFFECTIVE flag is defined.
    EXPECT_EQ(-1, fsetxattr(fd_, kName, invalid.data(), 20, 0));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(-1, fsetxattr(fd_, kName, valid.data(), 24, 0));
    EXPECT_EQ(EINVAL, errno);
    std::array<uint8_t, 24> read {};
    ASSERT_EQ(20, fgetxattr(fd_, kName, read.data(), read.size()));
    EXPECT_EQ(kRev2 | 1, word(read.data()));
}

TEST_F(MountFilecapsTest, DataWriteRemovesCapabilitiesEvenForPrivilegedWriter) {
    set(capability(kRev2), 20);
    ASSERT_FALSE(HasFatalFailure());
    ASSERT_EQ(1, pwrite(fd_, "x", 1, 0)) << strerror(errno);
    EXPECT_EQ(-1, fgetxattr(fd_, kName, nullptr, 0));
    EXPECT_EQ(ENODATA, errno);
}

TEST_F(MountFilecapsTest, CapabilityOnlyRemovalDoesNotSynthesizeAttributeEvent) {
    set(capability(kRev2), 20);
    ASSERT_FALSE(HasFatalFailure());
    int notify = inotify_init1(IN_CLOEXEC | IN_NONBLOCK);
    ASSERT_GE(notify, 0) << strerror(errno);
    int watch = inotify_add_watch(notify, path_.c_str(), IN_ATTRIB | IN_MODIFY);
    if (watch < 0) {
        int error = errno;
        close(notify);
        FAIL() << strerror(error);
    }
    ssize_t written = pwrite(fd_, "x", 1, 0);
    int write_error = errno;
    alignas(inotify_event) char events[1024];
    ssize_t count = read(notify, events, sizeof(events));
    int read_error = errno;
    close(notify);
    ASSERT_EQ(1, written) << strerror(write_error);
    ASSERT_GT(count, 0) << strerror(read_error);
    bool attrib = false;
    bool modify = false;
    for (size_t offset = 0; offset < static_cast<size_t>(count);) {
        ASSERT_LE(offset + sizeof(inotify_event), static_cast<size_t>(count));
        auto* event = reinterpret_cast<const inotify_event*>(events + offset);
        ASSERT_LE(offset + sizeof(*event) + event->len, static_cast<size_t>(count));
        EXPECT_EQ(watch, event->wd);
        if (event->mask & IN_ATTRIB) attrib = true;
        if (event->mask & IN_MODIFY) {
            modify = true;
        }
        offset += sizeof(*event) + event->len;
    }
    EXPECT_FALSE(attrib);
    EXPECT_TRUE(modify);
    EXPECT_EQ(-1, fgetxattr(fd_, kName, nullptr, 0));
    EXPECT_EQ(ENODATA, errno);
}

TEST_F(MountFilecapsTest, ModeRemovalNotifiesAttributesBeforeModifiedData) {
    ASSERT_EQ(0, fchown(fd_, 1000, 1000)) << strerror(errno);
    ASSERT_EQ(0, fchmod(fd_, 06777)) << strerror(errno);
    int notify = inotify_init1(IN_CLOEXEC | IN_NONBLOCK);
    ASSERT_GE(notify, 0) << strerror(errno);
    int watch = inotify_add_watch(notify, path_.c_str(), IN_ATTRIB | IN_MODIFY);
    if (watch < 0) {
        int error = errno;
        close(notify);
        FAIL() << strerror(error);
    }
    pid_t child = fork();
    if (child < 0) {
        int error = errno;
        close(notify);
        FAIL() << strerror(error);
    }
    if (child == 0) {
        if (setgid(1000) || setuid(1000)) _exit(2);
        _exit(pwrite(fd_, "x", 1, 0) == 1 ? 0 : 3);
    }
    int status = 0;
    pid_t waited = waitpid(child, &status, 0);
    alignas(inotify_event) char events[1024];
    ssize_t count = read(notify, events, sizeof(events));
    int read_error = errno;
    close(notify);
    ASSERT_EQ(child, waited);
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(0, WEXITSTATUS(status));
    ASSERT_GT(count, 0) << strerror(read_error);
    bool attrib = false;
    bool modify = false;
    for (size_t offset = 0; offset < static_cast<size_t>(count);) {
        ASSERT_LE(offset + sizeof(inotify_event), static_cast<size_t>(count));
        auto* event = reinterpret_cast<const inotify_event*>(events + offset);
        ASSERT_LE(offset + sizeof(*event) + event->len, static_cast<size_t>(count));
        EXPECT_EQ(watch, event->wd);
        if (event->mask & IN_ATTRIB) attrib = true;
        if (event->mask & IN_MODIFY) {
            EXPECT_TRUE(attrib);
            modify = true;
        }
        offset += sizeof(*event) + event->len;
    }
    EXPECT_TRUE(attrib);
    EXPECT_TRUE(modify);
    struct stat state {};
    ASSERT_EQ(0, fstat(fd_, &state));
    EXPECT_EQ(0, state.st_mode & (S_ISUID | S_ISGID));
}

TEST_F(MountFilecapsTest, SameSizeTruncateAndNoChangeChownStillRemoveCapabilities) {
    set(capability(kRev2), 20);
    ASSERT_FALSE(HasFatalFailure());
    ASSERT_EQ(0, ftruncate(fd_, 0)) << strerror(errno);
    EXPECT_EQ(-1, fgetxattr(fd_, kName, nullptr, 0));
    EXPECT_EQ(ENODATA, errno);
    set(capability(kRev2), 20);
    ASSERT_FALSE(HasFatalFailure());
    ASSERT_EQ(0, fchown(fd_, static_cast<uid_t>(-1), static_cast<gid_t>(-1)))
        << strerror(errno);
    EXPECT_EQ(-1, fgetxattr(fd_, kName, nullptr, 0));
    EXPECT_EQ(ENODATA, errno);
}
} // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
