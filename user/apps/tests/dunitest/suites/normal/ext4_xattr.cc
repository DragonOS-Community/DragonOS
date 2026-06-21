#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/statfs.h>
#include <sys/xattr.h>
#include <unistd.h>

#include <string>

namespace {

constexpr long kExt4SuperMagic = 0xEF53;

class TempFile {
  public:
    TempFile() {
        char tmpl[] = "/root/dunitest_ext4_xattr_XXXXXX";
        fd_ = mkstemp(tmpl);
        if (fd_ >= 0) {
            path_ = tmpl;
        }
    }

    ~TempFile() {
        if (fd_ >= 0) {
            close(fd_);
        }
        if (!path_.empty()) {
            unlink(path_.c_str());
        }
    }

    TempFile(const TempFile&) = delete;
    TempFile& operator=(const TempFile&) = delete;

    bool valid() const {
        return fd_ >= 0;
    }

    const char* path() const {
        return path_.c_str();
    }

  private:
    std::string path_;
    int fd_ = -1;
};

void ExpectValue(const char* path, const char* name, const char* expected) {
    char buf[32] = {};
    errno = 0;
    ssize_t n = getxattr(path, name, buf, sizeof(buf));
    ASSERT_EQ(static_cast<ssize_t>(strlen(expected)), n)
        << "getxattr failed errno=" << errno << " (" << strerror(errno) << ")";
    EXPECT_EQ(0, memcmp(buf, expected, strlen(expected)));
}

bool IsXattrUnsupported(int err) {
    return err == ENOTSUP || err == ENOSYS || err == EOPNOTSUPP;
}

}  // namespace

TEST(Ext4Xattr, CreateReplaceFlagsAndFailurePreserveValue) {
    struct statfs st = {};
    ASSERT_EQ(0, statfs("/root", &st)) << "statfs(/root) failed: " << strerror(errno);
    if (st.f_type != kExt4SuperMagic) {
        GTEST_SKIP() << "/root is not ext4, f_type=0x" << std::hex << st.f_type;
    }

    TempFile file;
    ASSERT_TRUE(file.valid()) << "mkstemp failed: " << strerror(errno);

    constexpr const char* kName = "user.dragonos_ext4_flags";

    errno = 0;
    if (setxattr(file.path(), kName, "base", 4, 0) != 0) {
        if (IsXattrUnsupported(errno)) {
            GTEST_SKIP() << "xattr is not supported on ext4 path";
        }
        FAIL() << "initial setxattr failed errno=" << errno << " (" << strerror(errno) << ")";
    }
    ExpectValue(file.path(), kName, "base");

    errno = 0;
    EXPECT_EQ(-1, setxattr(file.path(), kName, "create", 6, XATTR_CREATE));
    EXPECT_EQ(EEXIST, errno);
    ExpectValue(file.path(), kName, "base");

    errno = 0;
    ASSERT_EQ(0, setxattr(file.path(), kName, "replace", 7, XATTR_REPLACE))
        << "replace existing failed errno=" << errno << " (" << strerror(errno) << ")";
    ExpectValue(file.path(), kName, "replace");

    constexpr const char* kMissing = "user.dragonos_ext4_missing";
    errno = 0;
    EXPECT_EQ(-1, setxattr(file.path(), kMissing, "value", 5, XATTR_REPLACE));
    EXPECT_EQ(ENODATA, errno);
    ExpectValue(file.path(), kName, "replace");

    errno = 0;
    ASSERT_EQ(0, setxattr(file.path(), kMissing, "created", 7, XATTR_CREATE))
        << "create missing failed errno=" << errno << " (" << strerror(errno) << ")";
    ExpectValue(file.path(), kMissing, "created");
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
