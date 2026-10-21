#include <gtest/gtest.h>

#include <string>

#include "fuse_gtest_common.h"

namespace {

constexpr uint32_t kAtime = 1U << 4;
constexpr uint32_t kMtime = 1U << 5;

class FuseAtime : public ::testing::Test {
protected:
    void SetUp() override {
        char name[] = "/tmp/fuse_atime_XXXXXX";
        char *directory = mkdtemp(name);
        ASSERT_NE(nullptr, directory) << strerror(errno);
        mountpoint_ = directory;
        fd_ = open("/dev/fuse", O_RDWR);
        ASSERT_GE(fd_, 0) << strerror(errno);
        args_.fd = fd_;
        args_.stop = &stop_;
        args_.init_done = &initialized_;
        args_.enable_write_ops = 1;
        args_.stop_on_destroy = 1;
        args_.setattr_count = &setattr_count_;
        args_.last_setattr_valid = &last_setattr_valid_;
        args_.last_setattr_request = &last_setattr_request_;
        ASSERT_EQ(0, pthread_create(&thread_, nullptr, fuse_daemon_thread, &args_));
        started_ = true;
        char options[128];
        snprintf(options, sizeof(options), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd_);
        ASSERT_EQ(0, mount("none", mountpoint_.c_str(), "fuse", MS_STRICTATIME, options))
            << strerror(errno);
        mounted_ = true;
        ASSERT_EQ(0, fuseg_wait_init(&initialized_));
    }

    void TearDown() override {
        if (mounted_) {
            EXPECT_EQ(0, umount(mountpoint_.c_str())) << strerror(errno);
        }
        stop_ = 1;
        if (fd_ >= 0) {
            close(fd_);
        }
        if (started_) {
            pthread_join(thread_, nullptr);
        }
        if (!mountpoint_.empty()) {
            rmdir(mountpoint_.c_str());
        }
    }

    void ReadAndRefresh() {
        const std::string file = mountpoint_ + "/hello.txt";
        for (int attempt = 0; attempt < 4; ++attempt) {
            int fd = open(file.c_str(), O_RDONLY);
            ASSERT_GE(fd, 0) << strerror(errno);
            char data[64] = {};
            ssize_t count = pread(fd, data, sizeof(data), 0);
            EXPECT_EQ(16, count);
            if (count == 16) {
                EXPECT_EQ(0, memcmp(data, "hello from fuse\n", 16));
            }
            EXPECT_EQ(0, pread(fd, data, sizeof(data), 4096));
            EXPECT_EQ(0, close(fd));
            // Zero attribute TTL in the daemon forces a fresh GETATTR. This
            // also checks that daemon metadata replacement preserves policy.
            struct stat info;
            ASSERT_EQ(0, stat(file.c_str(), &info)) << strerror(errno);
        }
        DIR *directory = opendir(mountpoint_.c_str());
        ASSERT_NE(nullptr, directory) << strerror(errno);
        bool found = false;
        while (struct dirent *entry = readdir(directory)) {
            found = found || strcmp(entry->d_name, "hello.txt") == 0;
        }
        EXPECT_TRUE(found);
        EXPECT_EQ(0, closedir(directory));
    }

    std::string mountpoint_;
    int fd_ = -1;
    bool started_ = false;
    bool mounted_ = false;
    pthread_t thread_ = {};
    volatile int stop_ = 0;
    volatile int initialized_ = 0;
    volatile uint32_t setattr_count_ = 0;
    volatile uint32_t last_setattr_valid_ = 0;
    struct fuse_daemon_args args_ = {};
    struct fuse_setattr_in last_setattr_request_ = {};
};

TEST_F(FuseAtime, ReadsLeaveAutomaticAtimeToDaemon) {
    ReadAndRefresh();
    EXPECT_EQ(0U, setattr_count_) << "last SETATTR valid=" << last_setattr_valid_;
}

TEST_F(FuseAtime, ExplicitTimestampsStillReachDaemonAndPreservePolicy) {
    const std::string file = mountpoint_ + "/hello.txt";
    const struct timespec both[2] = {{123, 456}, {789, 123}};
    ASSERT_EQ(0, utimensat(AT_FDCWD, file.c_str(), both, 0)) << strerror(errno);
    EXPECT_EQ(1U, setattr_count_);
    EXPECT_EQ(kAtime | kMtime, last_setattr_valid_ & (kAtime | kMtime));
    EXPECT_EQ(0U, last_setattr_valid_ & FATTR_SIZE);
    EXPECT_EQ(123U, last_setattr_request_.atime);
    EXPECT_EQ(456U, last_setattr_request_.atimensec);
    EXPECT_EQ(789U, last_setattr_request_.mtime);
    EXPECT_EQ(123U, last_setattr_request_.mtimensec);
    const uint32_t after_explicit = setattr_count_;
    ReadAndRefresh();
    EXPECT_EQ(after_explicit, setattr_count_);

    // Explicit atime-only updates must not be suppressed by S_NOATIME.
    const struct timespec atime_only[2] = {{321, 654}, {0, UTIME_OMIT}};
    ASSERT_EQ(0, utimensat(AT_FDCWD, file.c_str(), atime_only, 0)) << strerror(errno);
    EXPECT_EQ(after_explicit + 1, setattr_count_);
    EXPECT_NE(0U, last_setattr_valid_ & kAtime);
    EXPECT_EQ(0U, last_setattr_valid_ & kMtime);
    EXPECT_EQ(321U, last_setattr_request_.atime);
    EXPECT_EQ(654U, last_setattr_request_.atimensec);
    const uint32_t after_second = setattr_count_;
    ReadAndRefresh();
    EXPECT_EQ(after_second, setattr_count_);
}

class FuseAtimeReadOnly : public FuseAtime {
protected:
    void SetUp() override {
        args_.setattr_error = EROFS;
        FuseAtime::SetUp();
    }
};

TEST_F(FuseAtimeReadOnly, ExplicitTimestampErrorsAreNotSuppressed) {
    ReadAndRefresh();
    EXPECT_EQ(0U, setattr_count_);
    const std::string file = mountpoint_ + "/hello.txt";
    const struct timespec both[2] = {{123, 456}, {789, 123}};
    errno = 0;
    EXPECT_EQ(-1, utimensat(AT_FDCWD, file.c_str(), both, 0));
    EXPECT_EQ(EROFS, errno);
    EXPECT_EQ(1U, setattr_count_);
    EXPECT_EQ(kAtime | kMtime, last_setattr_valid_ & (kAtime | kMtime));
    EXPECT_EQ(123U, last_setattr_request_.atime);
    EXPECT_EQ(456U, last_setattr_request_.atimensec);
    EXPECT_EQ(789U, last_setattr_request_.mtime);
    EXPECT_EQ(123U, last_setattr_request_.mtimensec);
    ReadAndRefresh();
    EXPECT_EQ(1U, setattr_count_);
}

} // namespace

int main(int argc, char **argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
