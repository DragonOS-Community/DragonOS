#include "loop_ext4_test_support.h"

#include <atomic>
#include <sys/xattr.h>
#include <thread>
#include <vector>

namespace {

class Ext4SharedXattr : public testing::Test {
  protected:
    void SetUp() override {
        ASSERT_NO_FATAL_FAILURE(disk_.SetUp("ext4_shared_xattr.img", "/tmp",
                                           "ext4_shared_xattr"));
        ASSERT_NO_FATAL_FAILURE(disk_.Mount());
        for (const char* file : {"a", "b", "c"}) {
            ExpectValue(file, "user.alpha", std::string(1000, 'a'));
            ExpectValue(file, "user.beta", std::string(1000, 'b'));
            ExpectBlocks(file, 8);
        }
    }

    std::string Path(const char* file) const {
        return disk_.mount_point() + "/" + file;
    }

    void ExpectValue(const char* file, const char* name, const std::string& value) {
        std::vector<char> actual(value.size() + 1);
        ASSERT_EQ(static_cast<ssize_t>(value.size()),
                  getxattr(Path(file).c_str(), name, actual.data(), actual.size()))
            << file << ": " << strerror(errno);
        EXPECT_EQ(value, std::string(actual.data(), value.size())) << file;
    }

    void ExpectMissing(const char* file, const char* name) {
        errno = 0;
        EXPECT_EQ(-1, getxattr(Path(file).c_str(), name, nullptr, 0));
        EXPECT_EQ(ENODATA, errno);
    }

    void ExpectBlocks(const char* file, blkcnt_t blocks) {
        struct stat st = {};
        ASSERT_EQ(0, stat(Path(file).c_str(), &st));
        EXPECT_EQ(blocks, st.st_blocks) << file;
    }

    void ExpectPeers() {
        for (const char* file : {"b", "c"}) {
            ExpectValue(file, "user.alpha", std::string(1000, 'a'));
            ExpectValue(file, "user.beta", std::string(1000, 'b'));
            ExpectMissing(file, "user.gamma");
            ExpectBlocks(file, 8);
        }
    }

    void Remount() {
        ASSERT_NO_FATAL_FAILURE(disk_.Unmount());
        ASSERT_NO_FATAL_FAILURE(disk_.Mount());
    }

    dunitest::LoopExt4 disk_;
};

TEST_F(Ext4SharedXattr, SetAndReplacePreservePeers) {
    const std::string value(1100, 'x');
    ASSERT_EQ(0, setxattr(Path("a").c_str(), "user.alpha", value.data(), value.size(), 0));
    ExpectValue("a", "user.alpha", value);
    ExpectPeers();
    const std::string replacement(900, 'r');
    ASSERT_EQ(0, setxattr(Path("a").c_str(), "user.alpha", replacement.data(),
                          replacement.size(), XATTR_REPLACE));
    Remount();
    ExpectValue("a", "user.alpha", replacement);
    ExpectBlocks("a", 8);
    ExpectPeers();
}

TEST_F(Ext4SharedXattr, CreatePreservesPeers) {
    const std::string value(500, 'g');
    ASSERT_EQ(0, setxattr(Path("a").c_str(), "user.gamma", value.data(), value.size(),
                          XATTR_CREATE));
    Remount();
    ExpectValue("a", "user.gamma", value);
    ExpectBlocks("a", 8);
    ExpectPeers();
}

TEST_F(Ext4SharedXattr, RemoveAndLastOwnerRelease) {
    ASSERT_EQ(0, removexattr(Path("a").c_str(), "user.alpha"));
    ExpectMissing("a", "user.alpha");
    ExpectValue("a", "user.beta", std::string(1000, 'b'));
    ExpectPeers();
    for (const char* file : {"b", "c"}) {
        ASSERT_EQ(0, removexattr(Path(file).c_str(), "user.alpha"));
        ASSERT_EQ(0, removexattr(Path(file).c_str(), "user.beta"));
    }
    ASSERT_EQ(0, removexattr(Path("a").c_str(), "user.beta"));
    Remount();
    for (const char* file : {"a", "b", "c"}) {
        ExpectMissing(file, "user.alpha");
        ExpectMissing(file, "user.beta");
        ExpectBlocks(file, 0);
    }
}

TEST_F(Ext4SharedXattr, RejectedUpdatesPreserveAllOwners) {
    const std::string oversized(4096, 'x');
    errno = 0;
    EXPECT_EQ(-1, setxattr(Path("a").c_str(), "user.alpha", "x", 1, XATTR_CREATE));
    EXPECT_EQ(EEXIST, errno);
    errno = 0;
    EXPECT_EQ(-1, setxattr(Path("a").c_str(), "user.missing", "x", 1, XATTR_REPLACE));
    EXPECT_EQ(ENODATA, errno);
    EXPECT_EQ(-1, setxattr(Path("a").c_str(), "user.alpha", oversized.data(),
                         oversized.size(), 0));
    Remount();
    ExpectValue("a", "user.alpha", std::string(1000, 'a'));
    ExpectValue("a", "user.beta", std::string(1000, 'b'));
    ExpectPeers();
}

TEST_F(Ext4SharedXattr, ConcurrentOwnersAndUnlink) {
    std::atomic<int> failure{0};
    auto replace = [&](const char* file, char byte) {
        const std::string value(1100, byte);
        if (setxattr(Path(file).c_str(), "user.alpha", value.data(), value.size(),
                     XATTR_REPLACE) != 0) {
            failure.store(errno ? errno : EIO);
        }
    };
    std::thread first(replace, "a", 'x');
    std::thread second(replace, "b", 'y');
    first.join();
    second.join();
    ASSERT_EQ(0, failure.load());
    ExpectValue("a", "user.alpha", std::string(1100, 'x'));
    ExpectValue("b", "user.alpha", std::string(1100, 'y'));
    ExpectValue("c", "user.alpha", std::string(1000, 'a'));
    ASSERT_EQ(0, unlink(Path("b").c_str()));
    ASSERT_EQ(0, unlink(Path("c").c_str()));
    Remount();
    ExpectValue("a", "user.alpha", std::string(1100, 'x'));
    ExpectValue("a", "user.beta", std::string(1000, 'b'));
    ExpectBlocks("a", 8);
}

TEST_F(Ext4SharedXattr, ReadersOverlapDetachAndReattach) {
    ASSERT_EQ(0, removexattr(Path("a").c_str(), "user.beta"));
    const std::string value(1000, 'a');
    const auto path = Path("a");
    std::atomic<bool> done{false};
    std::atomic<int> failure{0};
    std::thread reader([&] {
        std::vector<char> actual(1001);
        do {
            ssize_t size = getxattr(path.c_str(), "user.alpha", actual.data(), actual.size());
            if (size == -1 && errno == ENODATA) {
                continue;
            }
            if (size != 1000 || std::string(actual.data(), 1000) != value) {
                failure.store(size < 0 ? errno : EIO);
                break;
            }
        } while (!done.load());
    });
    for (int i = 0; i < 32; ++i) {
        if (removexattr(path.c_str(), "user.alpha") != 0 ||
            setxattr(path.c_str(), "user.alpha", value.data(), value.size(),
                     XATTR_CREATE) != 0) {
            failure.store(errno ? errno : EIO);
            break;
        }
    }
    done.store(true);
    reader.join();
    ASSERT_EQ(0, failure.load());
    Remount();
    ExpectValue("a", "user.alpha", value);
    ExpectBlocks("a", 8);
    ExpectPeers();
}

} // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
