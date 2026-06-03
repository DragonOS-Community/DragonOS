#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#include <string>

namespace {

constexpr const char* kDevPtmx = "/dev/ptmx";
constexpr const char* kDevPtsPtmx = "/dev/pts/ptmx";
constexpr const char* kScratchLink = "/dev/dunitest_devfs_unlink_link";

std::string readlink_string(const char* path) {
    char buf[256];
    ssize_t len = readlink(path, buf, sizeof(buf) - 1);
    if (len < 0) {
        return {};
    }
    buf[len] = '\0';
    return std::string(buf, static_cast<size_t>(len));
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
            was_symlink_ = S_ISLNK(st.st_mode);
            if (was_symlink_) {
                target_ = readlink_string(kDevPtmx);
            }
        }
    }

    ~DevPtmxRestorer() {
        restore();
    }

    void require_original_symlink() const {
        ASSERT_TRUE(existed_) << "/dev/ptmx must exist before this test";
        ASSERT_TRUE(was_symlink_) << "/dev/ptmx must be a symlink before this test";
        ASSERT_FALSE(target_.empty()) << "failed to capture original /dev/ptmx target";
    }

    const std::string& target() const {
        return target_;
    }

    void restore() const {
        if (!can_restore()) {
            return;
        }

        unlink(kDevPtmx);
        if (existed_ && was_symlink_ && !target_.empty()) {
            int ret = symlink(target_.c_str(), kDevPtmx);
            (void)ret;
        }
    }

private:
    bool can_restore() const {
        return !existed_ || (was_symlink_ && !target_.empty());
    }

    bool existed_ = false;
    bool was_symlink_ = false;
    std::string target_;
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
    restorer.require_original_symlink();

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
    ASSERT_EQ(restorer.target(), readlink_string(kDevPtmx));
    expect_open_ptmx(kDevPtmx);
}

TEST(DevfsPtmxUnlink, RecreateDevPtmxSymlinkAfterUnlink) {
    DevPtmxRestorer restorer;
    restorer.require_original_symlink();

    ASSERT_EQ(-1, symlink(kDevPtsPtmx, kDevPtmx))
        << "symlink unexpectedly replaced existing /dev/ptmx";
    ASSERT_EQ(EEXIST, errno) << "unexpected errno for existing /dev/ptmx: " << strerror(errno);

    ASSERT_EQ(0, unlink(kDevPtmx)) << "unlink(/dev/ptmx) failed: errno=" << errno << " ("
                                  << strerror(errno) << ")";
    ASSERT_EQ(0, symlink(kDevPtsPtmx, kDevPtmx))
        << "symlink(/dev/pts/ptmx, /dev/ptmx) failed: errno=" << errno << " ("
        << strerror(errno) << ")";

    struct stat st = {};
    ASSERT_EQ(0, lstat(kDevPtmx, &st)) << "lstat(/dev/ptmx) failed: errno=" << errno << " ("
                                      << strerror(errno) << ")";
    ASSERT_TRUE(S_ISLNK(st.st_mode)) << "/dev/ptmx should be a symlink";
    ASSERT_EQ(std::string(kDevPtsPtmx), readlink_string(kDevPtmx));

    expect_open_ptmx(kDevPtmx);

    restorer.restore();
    ASSERT_EQ(restorer.target(), readlink_string(kDevPtmx));
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
