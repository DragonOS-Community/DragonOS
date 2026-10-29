#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <unistd.h>

namespace {

constexpr const char* kDevPtmx = "/dev/ptmx";
constexpr const char* kDevPtsPtmx = "/dev/pts/ptmx";
constexpr const char* kScratchLink = "/dev/dunitest_devfs_unlink_link";

bool is_ptmx_char_device(const struct stat& st) {
    return S_ISCHR(st.st_mode) && major(st.st_rdev) == 5 && minor(st.st_rdev) == 2;
}

void expect_open_ptmx(const char* path) {
    int fd = open(path, O_RDWR | O_NOCTTY);
    ASSERT_GE(fd, 0) << "open(" << path << ") failed: errno=" << errno << " ("
                     << strerror(errno) << ")";
    ASSERT_EQ(0, close(fd)) << "close(" << path << ") failed: errno=" << errno << " ("
                            << strerror(errno) << ")";
}

class DevPtmxRestorer {
public:
    DevPtmxRestorer() {
        struct stat st = {};
        existed_ = lstat(kDevPtmx, &st) == 0;
        if (existed_) {
            mode_ = st.st_mode;
            rdev_ = st.st_rdev;
        }
    }

    ~DevPtmxRestorer() {
        restore();
    }

    void require_original_ptmx() const {
        ASSERT_TRUE(existed_) << "/dev/ptmx must exist before this test";
        ASSERT_TRUE(S_ISCHR(mode_)) << "/dev/ptmx must be a character device before this test";
        ASSERT_EQ(5u, major(rdev_)) << "/dev/ptmx major mismatch";
        ASSERT_EQ(2u, minor(rdev_)) << "/dev/ptmx minor mismatch";
    }

    void restore() const {
        if (!can_restore()) {
            return;
        }

        unlink(kDevPtmx);
        if (existed_) {
            int ret = mknod(kDevPtmx, (mode_ & 07777) | S_IFCHR, rdev_);
            (void)ret;
        }
    }

private:
    bool can_restore() const {
        return !existed_ || S_ISCHR(mode_);
    }

    bool existed_ = false;
    mode_t mode_ = 0;
    dev_t rdev_ = 0;
};

void remove_scratch_link() {
    unlink(kScratchLink);
}

class ScratchLinkRestorer {
public:
    ScratchLinkRestorer() {
        remove_scratch_link();
    }

    ~ScratchLinkRestorer() {
        remove_scratch_link();
    }
};

}  // namespace

TEST(DevfsPtmxUnlink, UnlinkDevPtmxRemovesOnlyDevfsEntry) {
    DevPtmxRestorer restorer;
    restorer.require_original_ptmx();

    expect_open_ptmx(kDevPtmx);
    expect_open_ptmx(kDevPtsPtmx);

    ASSERT_EQ(0, unlink(kDevPtmx)) << "unlink(/dev/ptmx) failed: errno=" << errno << " ("
                                  << strerror(errno) << ")";

    struct stat st = {};
    ASSERT_EQ(-1, lstat(kDevPtmx, &st)) << "/dev/ptmx still exists after unlink";
    ASSERT_EQ(ENOENT, errno) << "unexpected errno after lstat(/dev/ptmx): " << strerror(errno);

    ASSERT_EQ(0, stat(kDevPtsPtmx, &st)) << "stat(/dev/pts/ptmx) failed after unlink: errno="
                                        << errno << " (" << strerror(errno) << ")";
    expect_open_ptmx(kDevPtsPtmx);

    restorer.restore();
    ASSERT_EQ(0, lstat(kDevPtmx, &st)) << "lstat(/dev/ptmx) failed after restore: errno="
                                      << errno << " (" << strerror(errno) << ")";
    ASSERT_TRUE(is_ptmx_char_device(st)) << "/dev/ptmx should be restored as c 5:2";
    expect_open_ptmx(kDevPtmx);
}

TEST(DevfsPtmxUnlink, RecreateDevPtmxSpecialNodeAfterUnlink) {
    DevPtmxRestorer restorer;
    restorer.require_original_ptmx();

    ASSERT_EQ(-1, mknod(kDevPtmx, S_IFCHR | 0666, makedev(5, 2)))
        << "mknod unexpectedly replaced existing /dev/ptmx";
    ASSERT_EQ(EEXIST, errno) << "unexpected errno for existing /dev/ptmx: " << strerror(errno);

    ASSERT_EQ(0, unlink(kDevPtmx)) << "unlink(/dev/ptmx) failed: errno=" << errno << " ("
                                  << strerror(errno) << ")";
    ASSERT_EQ(0, mknod(kDevPtmx, S_IFCHR | 0666, makedev(5, 2)))
        << "mknod(/dev/ptmx) failed: errno=" << errno << " ("
        << strerror(errno) << ")";

    struct stat st = {};
    ASSERT_EQ(0, lstat(kDevPtmx, &st)) << "lstat(/dev/ptmx) failed: errno=" << errno << " ("
                                      << strerror(errno) << ")";
    ASSERT_TRUE(is_ptmx_char_device(st)) << "/dev/ptmx should be c 5:2";

    expect_open_ptmx(kDevPtmx);

    restorer.restore();
    ASSERT_EQ(0, lstat(kDevPtmx, &st)) << "lstat(/dev/ptmx) failed after restore: errno="
                                      << errno << " (" << strerror(errno) << ")";
    ASSERT_TRUE(is_ptmx_char_device(st)) << "/dev/ptmx should be restored as c 5:2";
    expect_open_ptmx(kDevPtmx);
}

TEST(DevfsPtmxUnlink, UnlinkScratchDevfsSymlink) {
    ScratchLinkRestorer scratch_restorer;

    ASSERT_EQ(0, symlink(kDevPtsPtmx, kScratchLink))
        << "symlink scratch link failed: errno=" << errno << " (" << strerror(errno) << ")";
    expect_open_ptmx(kScratchLink);

    ASSERT_EQ(0, unlink(kScratchLink)) << "unlink scratch link failed: errno=" << errno << " ("
                                      << strerror(errno) << ")";

    struct stat st = {};
    ASSERT_EQ(-1, lstat(kScratchLink, &st)) << "scratch link still exists after unlink";
    ASSERT_EQ(ENOENT, errno) << "unexpected errno after lstat scratch link: " << strerror(errno);
}

TEST(DevfsPtmxUnlink, UnlinkDevDirectoryFails) {
    struct stat st = {};
    ASSERT_EQ(-1, unlink("/dev/pts")) << "unlink(/dev/pts) unexpectedly succeeded";
    ASSERT_EQ(EISDIR, errno) << "unexpected errno for unlink(/dev/pts): " << strerror(errno);
    ASSERT_EQ(0, stat("/dev/pts", &st)) << "stat(/dev/pts) failed: errno=" << errno << " ("
                                       << strerror(errno) << ")";
    ASSERT_TRUE(S_ISDIR(st.st_mode)) << "/dev/pts should remain a directory";
}

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
