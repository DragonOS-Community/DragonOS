#include <gtest/gtest.h>

#include <fcntl.h>
#include <linux/mount.h>
#include <sched.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include <cerrno>
#include <cstdlib>
#include <cstring>
#include <string>
#include <sstream>

namespace {

// Every view is a real mount of the device tag, never a bind substitute. Tests
// only unmount their own mkdtemp paths. For session-last-owner coverage, the
// runner must first unmount any separate export and run this binary locally.
class Fd {
 public:
    explicit Fd(int fd = -1) : fd_(fd) {}
    ~Fd() { Reset(); }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int Get() const { return fd_; }
    void Reset(int fd = -1) {
        if (fd_ >= 0) close(fd_);
        fd_ = fd;
    }
 private:
    int fd_;
};

std::string Setting(const char* name, const char* fallback) {
    const char* value = getenv(name);
    return value && *value ? value : fallback;
}

int Configure(int fd, unsigned int command, const char* key = nullptr,
              const char* value = nullptr) {
    return static_cast<int>(syscall(SYS_fsconfig, fd, command, key, value, 0));
}

unsigned long long MountId(const std::string& path) {
    Fd info(open("/proc/self/mountinfo", O_RDONLY | O_CLOEXEC));
    if (info.Get() < 0) return 0;
    std::string contents;
    char chunk[4096];
    for (;;) {
        ssize_t count = read(info.Get(), chunk, sizeof(chunk));
        if (count < 0 && errno == EINTR) continue;
        if (count < 0) return 0;
        if (count == 0) break;
        contents.append(chunk, static_cast<size_t>(count));
        if (contents.size() > 1024 * 1024) return 0;
    }
    std::istringstream lines(contents);
    std::string line;
    while (std::getline(lines, line)) {
        std::istringstream fields(line);
        unsigned long long id;
        std::string parent, device, root, point;
        if (fields >> id >> parent >> device >> root >> point && point == path) return id;
    }
    return 0;
}

class VirtiofsMultimount : public ::testing::Test {
 protected:
    std::string base_, a_, b_, c_, tag_, file_;
    bool a_mounted_ = false, b_mounted_ = false, c_mounted_ = false;

    void SetUp() override {
        alarm(60);
        tag_ = Setting("DUNITEST_VIRTIOFS_TAG", "hostshare");
        char pattern[] = "/tmp/virtiofs_multimount_XXXXXX";
        char* created = mkdtemp(pattern);
        ASSERT_NE(nullptr, created) << strerror(errno);
        base_ = created;
        a_ = base_ + "/a";
        b_ = base_ + "/b";
        c_ = base_ + "/c";
        ASSERT_EQ(0, mkdir(a_.c_str(), 0700));
        ASSERT_EQ(0, mkdir(b_.c_str(), 0700));
        ASSERT_EQ(0, mkdir(c_.c_str(), 0700));
        if (mount(tag_.c_str(), a_.c_str(), "virtiofs", 0, "dax=never") != 0) {
            const int error = errno;
            if (Setting("DUNITEST_REQUIRE_VIRTIOFS", "0") != "1"
                // Linux virtio_fs_get_tree returns EINVAL for a missing tag.
                // Required-device runs never turn mount failures into skips.
                && (error == ENODEV || error == ENOENT || error == EINVAL || error == EPERM)) {
                GTEST_SKIP() << "real virtiofs device unavailable: " << strerror(error);
            }
            FAIL() << "required virtiofs mount failed: " << strerror(error);
        }
        a_mounted_ = true;
        // The host export is disposable test storage, but the filename is
        // unique to this invocation and cleanup never removes other contents.
        file_ = ".dunitest-multimount-" + base_.substr(base_.find_last_of('/') + 1);
        Fd fd(open((a_ + "/" + file_).c_str(), O_RDWR | O_CREAT | O_EXCL | O_CLOEXEC, 0600));
        ASSERT_GE(fd.Get(), 0) << strerror(errno);
        const char initial[] = "initial";
        ASSERT_EQ(static_cast<ssize_t>(sizeof(initial)), write(fd.Get(), initial, sizeof(initial)));
    }

    void TearDown() override {
        // Remove only this test's own file before dropping the final writable
        // view. Normal cleanup must work; lazy detach is failure cleanup only.
        bool retry_unlink = false;
        if (!file_.empty()) {
            if (!a_mounted_ && !b_mounted_ && !c_mounted_
                && mount(tag_.c_str(), a_.c_str(), "virtiofs", 0, nullptr) == 0) {
                a_mounted_ = true;
            }
            const std::string view = a_mounted_ ? a_ : b_mounted_ ? b_ : c_mounted_ ? c_ : "";
            retry_unlink = view.empty()
                || (unlink((view + "/" + file_).c_str()) != 0 && errno != ENOENT);
        }
        Cleanup(c_, c_mounted_);
        Cleanup(b_, b_mounted_);
        Cleanup(a_, a_mounted_);
        if (retry_unlink) {
            // A failed readonly test may leave no writable view. Retire that
            // SB before creating a fresh writable one to remove our own file.
            if (mount(tag_.c_str(), a_.c_str(), "virtiofs", 0, nullptr) == 0) {
                a_mounted_ = true;
                const int result = unlink((a_ + "/" + file_).c_str());
                EXPECT_TRUE(result == 0 || errno == ENOENT) << strerror(errno);
                Cleanup(a_, a_mounted_);
            } else {
                ADD_FAILURE() << "cannot reacquire writable cleanup view: " << strerror(errno);
            }
        }
        if (!c_.empty()) rmdir(c_.c_str());
        if (!b_.empty()) rmdir(b_.c_str());
        if (!a_.empty()) rmdir(a_.c_str());
        if (!base_.empty()) rmdir(base_.c_str());
        alarm(0);
    }

    void Cleanup(const std::string& path, bool& mounted) {
        if (!mounted) return;
        if (umount(path.c_str()) != 0) {
            ADD_FAILURE() << "ordinary cleanup " << path << ": " << strerror(errno);
            umount2(path.c_str(), MNT_DETACH);
        }
        mounted = false;
    }

    void MountB(unsigned long flags = 0, const char* options = nullptr) {
        ASSERT_EQ(0, mount(tag_.c_str(), b_.c_str(), "virtiofs", flags, options)) << strerror(errno);
        b_mounted_ = true;
    }

    void UnmountA() {
        ASSERT_EQ(0, umount(a_.c_str())) << strerror(errno);
        a_mounted_ = false;
    }

    int NewContext() {
        int fd = static_cast<int>(syscall(SYS_fsopen, "virtiofs", FSOPEN_CLOEXEC));
        if (fd < 0) return fd;
        if (Configure(fd, FSCONFIG_SET_STRING, "source", tag_.c_str()) != 0
            || Configure(fd, FSCONFIG_CMD_CREATE) != 0) {
            const int error = errno;
            close(fd);
            errno = error;
            return -1;
        }
        return fd;
    }

    void AttachContext(int context) {
        Fd mountfd(static_cast<int>(syscall(SYS_fsmount, context, FSMOUNT_CLOEXEC, 0)));
        ASSERT_GE(mountfd.Get(), 0) << strerror(errno);
        ASSERT_EQ(0, syscall(SYS_move_mount, mountfd.Get(), "", AT_FDCWD, b_.c_str(),
                             MOVE_MOUNT_F_EMPTY_PATH)) << strerror(errno);
        b_mounted_ = true;
    }

    void ReadInitial(const std::string& view) {
        Fd fd(open((view + "/" + file_).c_str(), O_RDONLY | O_CLOEXEC));
        ASSERT_GE(fd.Get(), 0) << strerror(errno);
        char data[8] = {};
        ASSERT_EQ(8, read(fd.Get(), data, sizeof(data))) << strerror(errno);
        ASSERT_EQ(0, memcmp(data, "initial", sizeof(data)));
    }

    void WaitChild(pid_t child) {
        int status = 0;
        ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
        ASSERT_TRUE(WIFEXITED(status));
        ASSERT_EQ(0, WEXITSTATUS(status));
    }
};

TEST_F(VirtiofsMultimount, TwoRealMountsShareIdentityAndRemainUsableIndependently) {
    ASSERT_NO_FATAL_FAILURE(MountB());
    struct stat a {}, b {};
    ASSERT_EQ(0, stat((a_ + "/" + file_).c_str(), &a));
    ASSERT_EQ(0, stat((b_ + "/" + file_).c_str(), &b));
    EXPECT_EQ(a.st_dev, b.st_dev);
    EXPECT_EQ(a.st_ino, b.st_ino);
    const auto a_id = MountId(a_);
    const auto b_id = MountId(b_);
    ASSERT_NE(0ULL, a_id);
    ASSERT_NE(0ULL, b_id);
    EXPECT_NE(a_id, b_id);
    ASSERT_NO_FATAL_FAILURE(UnmountA());
    ASSERT_NO_FATAL_FAILURE(ReadInitial(b_));
}

TEST_F(VirtiofsMultimount, BothViewsShareBufferedAndMappedWrites) {
    ASSERT_NO_FATAL_FAILURE(MountB());
    Fd first(open((a_ + "/" + file_).c_str(), O_RDWR | O_CLOEXEC));
    Fd second(open((b_ + "/" + file_).c_str(), O_RDWR | O_CLOEXEC));
    ASSERT_GE(first.Get(), 0);
    ASSERT_GE(second.Get(), 0);
    ASSERT_EQ(0, ftruncate(first.Get(), 4096));
    void* map = mmap(nullptr, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, first.Get(), 0);
    ASSERT_NE(MAP_FAILED, map) << strerror(errno);
    static const char from_b[] = "written-b";
    const ssize_t written = pwrite(second.Get(), from_b, sizeof(from_b), 0);
    EXPECT_EQ(static_cast<ssize_t>(sizeof(from_b)), written) << strerror(errno);
    if (written == static_cast<ssize_t>(sizeof(from_b))) {
        EXPECT_EQ(0, memcmp(map, from_b, sizeof(from_b)));
    }
    static const char from_a[] = "mapped-a";
    memcpy(map, from_a, sizeof(from_a));
    char data[sizeof(from_a)] = {};
    EXPECT_EQ(static_cast<ssize_t>(sizeof(data)), pread(second.Get(), data, sizeof(data), 0));
    EXPECT_EQ(0, memcmp(data, from_a, sizeof(data)));
    EXPECT_EQ(0, munmap(map, 4096));
}

TEST_F(VirtiofsMultimount, ReadonlyMountDoesNotMakeExistingWritableViewReadonly) {
    ASSERT_NO_FATAL_FAILURE(MountB(MS_RDONLY));
    errno = 0;
    Fd readonly(open((b_ + "/" + file_).c_str(), O_WRONLY | O_CLOEXEC));
    ASSERT_EQ(-1, readonly.Get());
    ASSERT_EQ(EROFS, errno);
    Fd writable(open((a_ + "/" + file_).c_str(), O_WRONLY | O_CLOEXEC));
    ASSERT_GE(writable.Get(), 0) << strerror(errno);
    ASSERT_EQ(1, write(writable.Get(), "x", 1));
}

TEST_F(VirtiofsMultimount, FirstReadonlySuperblockRemainsReadonlyForLaterWritableRequest) {
    ASSERT_NO_FATAL_FAILURE(UnmountA());
    ASSERT_EQ(0, mount(tag_.c_str(), a_.c_str(), "virtiofs", MS_RDONLY, "dax=never"))
        << strerror(errno);
    a_mounted_ = true;
    ASSERT_NO_FATAL_FAILURE(MountB());
    struct stat a {}, b {};
    ASSERT_EQ(0, stat(a_.c_str(), &a));
    ASSERT_EQ(0, stat(b_.c_str(), &b));
    EXPECT_EQ(a.st_dev, b.st_dev);
    for (const auto& view : {a_, b_}) {
        errno = 0;
        Fd fd(open((view + "/" + file_).c_str(), O_WRONLY | O_CLOEXEC));
        const int error = errno;
        EXPECT_EQ(-1, fd.Get());
        EXPECT_EQ(EROFS, error);
    }
    ASSERT_EQ(0, umount(b_.c_str())) << strerror(errno);
    b_mounted_ = false;
    ASSERT_NO_FATAL_FAILURE(UnmountA());
    // Retire the readonly SB before rebuilding a writable view for cleanup.
    ASSERT_EQ(0, mount(tag_.c_str(), a_.c_str(), "virtiofs", 0, "dax=never"))
        << strerror(errno);
    a_mounted_ = true;
    ASSERT_NO_FATAL_FAILURE(ReadInitial(a_));
}

TEST_F(VirtiofsMultimount, NamespaceCloneRetainsSharedConnectionAfterLocalUnmount) {
    ASSERT_NO_FATAL_FAILURE(MountB());
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (unshare(CLONE_NEWNS) != 0) _exit(10);
        if (mount(nullptr, "/", nullptr, MS_REC | MS_PRIVATE, nullptr) != 0) _exit(11);
        if (umount(a_.c_str()) != 0 || umount(b_.c_str()) != 0) _exit(12);
        if (mount(tag_.c_str(), c_.c_str(), "virtiofs", 0, nullptr) != 0) _exit(13);
        int fd = open((c_ + "/" + file_).c_str(), O_RDONLY | O_CLOEXEC);
        if (fd < 0) _exit(14);
        close(fd);
        if (umount(c_.c_str()) != 0) _exit(15);
        _exit(0);
    }
    ASSERT_NO_FATAL_FAILURE(WaitChild(child));
    ASSERT_NO_FATAL_FAILURE(ReadInitial(a_));
    ASSERT_NO_FATAL_FAILURE(ReadInitial(b_));
}

TEST_F(VirtiofsMultimount, CreatedContextKeepsRootAliveAfterLastOldMountUnmount) {
    Fd context(NewContext());
    ASSERT_GE(context.Get(), 0) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(UnmountA());
    ASSERT_NO_FATAL_FAILURE(AttachContext(context.Get()));
    ASSERT_NO_FATAL_FAILURE(ReadInitial(b_));
}

TEST_F(VirtiofsMultimount, ClosingUnusedCreatedContextAllowsFreshSession) {
    Fd context(NewContext());
    ASSERT_GE(context.Get(), 0) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(UnmountA());
    context.Reset();
    ASSERT_NO_FATAL_FAILURE(MountB());
    ASSERT_NO_FATAL_FAILURE(ReadInitial(b_));
}

TEST_F(VirtiofsMultimount, ClosingFirstCreatedContextReleasesNewDeviceSession) {
    ASSERT_NO_FATAL_FAILURE(UnmountA());
    Fd context(NewContext());
    ASSERT_GE(context.Get(), 0) << strerror(errno);
    context.Reset();
    ASSERT_NO_FATAL_FAILURE(MountB());
    ASSERT_NO_FATAL_FAILURE(ReadInitial(b_));
}

TEST_F(VirtiofsMultimount, FailedFsmountDoesNotConsumeCreatedRoot) {
    Fd context(NewContext());
    ASSERT_GE(context.Get(), 0) << strerror(errno);
    errno = 0;
    ASSERT_EQ(-1, syscall(SYS_fsmount, context.Get(), 0x80000000U, 0));
    ASSERT_EQ(EINVAL, errno);
    ASSERT_NO_FATAL_FAILURE(UnmountA());
    ASSERT_NO_FATAL_FAILURE(AttachContext(context.Get()));
    ASSERT_NO_FATAL_FAILURE(ReadInitial(b_));
}

TEST_F(VirtiofsMultimount, ConcurrentRealMountsAndUnmountsPreserveExistingView) {
    int gate[2];
    ASSERT_EQ(0, pipe2(gate, O_CLOEXEC));
    Fd reader(gate[0]), writer(gate[1]);
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(writer.Get());
        char token;
        if (read(reader.Get(), &token, 1) != 1) _exit(10);
        for (int i = 0; i < 8; ++i) {
            if (mount(tag_.c_str(), c_.c_str(), "virtiofs", 0, nullptr) != 0) _exit(11);
            if (umount(c_.c_str()) != 0) _exit(12);
        }
        _exit(0);
    }
    reader.Reset();
    EXPECT_EQ(1, write(writer.Get(), "x", 1));
    for (int i = 0; i < 8; ++i) {
        if (mount(tag_.c_str(), b_.c_str(), "virtiofs", 0, nullptr) != 0) {
            ADD_FAILURE() << "concurrent mount: " << strerror(errno);
            break;
        }
        b_mounted_ = true;
        if (umount(b_.c_str()) != 0) {
            ADD_FAILURE() << "concurrent unmount: " << strerror(errno);
            break;
        }
        b_mounted_ = false;
    }
    EXPECT_NO_FATAL_FAILURE(WaitChild(child));
    // Even a failed child may have left only our own temporary c mount behind.
    if (umount(c_.c_str()) != 0 && errno != EINVAL && errno != ENOENT) {
        ADD_FAILURE() << "concurrent child cleanup: " << strerror(errno);
        c_mounted_ = true;
    }
    ASSERT_NO_FATAL_FAILURE(ReadInitial(a_));
}

TEST_F(VirtiofsMultimount, ConcurrentFirstMountAndLastUnmountWithoutAnchor) {
    ASSERT_NO_FATAL_FAILURE(UnmountA());
    int gate[2];
    ASSERT_EQ(0, pipe2(gate, O_CLOEXEC));
    Fd reader(gate[0]), writer(gate[1]);
    // Both participants independently acquire and release real device mounts.
    // There is no permanent mount retaining the connection in this test.
    auto cycle = [&](const std::string& view, bool& mounted) {
        if (mount(tag_.c_str(), view.c_str(), "virtiofs", 0, nullptr) != 0) return 11;
        mounted = true;
        int result = 0;
        {
            Fd fd(open((view + "/" + file_).c_str(), O_RDONLY | O_CLOEXEC));
            char data[8] = {};
            if (fd.Get() < 0 || read(fd.Get(), data, sizeof(data)) != 8
                || memcmp(data, "initial", sizeof(data)) != 0) result = 12;
        }
        if (umount(view.c_str()) != 0) return 13;
        mounted = false;
        return result;
    };
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        alarm(45);
        close(writer.Get());
        char token;
        if (read(reader.Get(), &token, 1) != 1) _exit(10);
        bool mounted = false;
        for (int i = 0; i < 8; ++i) {
            int result = cycle(c_, mounted);
            if (result != 0) _exit(result);
        }
        _exit(0);
    }
    reader.Reset();
    EXPECT_EQ(1, write(writer.Get(), "x", 1));
    for (int i = 0; i < 8; ++i) {
        const int result = cycle(b_, b_mounted_);
        if (result != 0) {
            ADD_FAILURE() << "unanchored round " << i << " failed at stage " << result
                          << ": " << strerror(errno);
            break;
        }
    }
    EXPECT_NO_FATAL_FAILURE(WaitChild(child));
    if (umount(c_.c_str()) != 0 && errno != EINVAL && errno != ENOENT) {
        ADD_FAILURE() << "unanchored child cleanup: " << strerror(errno);
        c_mounted_ = true;
    }
    if (b_mounted_) Cleanup(b_, b_mounted_);
    // A final mount proves the racing final releases left the device reusable.
    ASSERT_NO_FATAL_FAILURE(MountB());
    ASSERT_NO_FATAL_FAILURE(ReadInitial(b_));
}

TEST_F(VirtiofsMultimount, InvalidOptionsDoNotDisturbExistingConnection) {
    errno = 0;
    ASSERT_EQ(-1, mount(tag_.c_str(), b_.c_str(), "virtiofs", 0, "dax=invalid"));
    ASSERT_EQ(EINVAL, errno);
    errno = 0;
    ASSERT_EQ(-1, mount(tag_.c_str(), b_.c_str(), "virtiofs", 0, "not_a_virtiofs_option=1"));
    ASSERT_EQ(EINVAL, errno);
    ASSERT_NO_FATAL_FAILURE(ReadInitial(a_));
    ASSERT_NO_FATAL_FAILURE(MountB());
}

TEST_F(VirtiofsMultimount, InvalidFirstMountOptionsLeaveDeviceReusable) {
    ASSERT_NO_FATAL_FAILURE(UnmountA());
    errno = 0;
    ASSERT_EQ(-1, mount(tag_.c_str(), b_.c_str(), "virtiofs", 0, "dax=invalid"));
    ASSERT_EQ(EINVAL, errno);
    ASSERT_NO_FATAL_FAILURE(MountB());
    ASSERT_NO_FATAL_FAILURE(ReadInitial(b_));
}

TEST_F(VirtiofsMultimount, FailedFirstMountOnNondirectoryReleasesDeviceSession) {
    ASSERT_NO_FATAL_FAILURE(UnmountA());
    const std::string target = c_ + "/not-a-directory";
    Fd file(open(target.c_str(), O_CREAT | O_EXCL | O_RDWR | O_CLOEXEC, 0600));
    ASSERT_GE(file.Get(), 0) << strerror(errno);
    file.Reset();
    // DragonOS do_new_mount builds the FS before root/target type checking in
    // prepare_subtree_with_root_dentry, so this exercises post-build rollback.
    const int result = mount(tag_.c_str(), target.c_str(), "virtiofs", 0, "dax=never");
    const int error = errno;
    if (result == 0) {
        EXPECT_EQ(0, umount(target.c_str())) << strerror(errno);
    }
    EXPECT_EQ(0, unlink(target.c_str())) << strerror(errno);
    ASSERT_EQ(-1, result);
    ASSERT_EQ(ENOTDIR, error);
    ASSERT_NO_FATAL_FAILURE(MountB());
    ASSERT_NO_FATAL_FAILURE(ReadInitial(b_));
}

TEST_F(VirtiofsMultimount, LastMountUnmountAllowsNewDeviceSession) {
    ASSERT_NO_FATAL_FAILURE(MountB());
    ASSERT_NO_FATAL_FAILURE(UnmountA());
    ASSERT_EQ(0, umount(b_.c_str())) << strerror(errno);
    b_mounted_ = false;
    ASSERT_NO_FATAL_FAILURE(MountB());
    ASSERT_NO_FATAL_FAILURE(ReadInitial(b_));
}

TEST_F(VirtiofsMultimount, LaterLegalDaxOptionReusesOriginalSuperblock) {
    // First mount requested dax=never; a later valid option does not configure
    // another connection or require a cache window for the existing instance.
    ASSERT_NO_FATAL_FAILURE(MountB(0, "dax=always"));
    struct stat a {}, b {};
    ASSERT_EQ(0, stat(a_.c_str(), &a));
    ASSERT_EQ(0, stat(b_.c_str(), &b));
    EXPECT_EQ(a.st_dev, b.st_dev);
    ASSERT_NO_FATAL_FAILURE(UnmountA());
    ASSERT_NO_FATAL_FAILURE(ReadInitial(b_));
}

TEST_F(VirtiofsMultimount, OpenFilePreventsOnlyItsOwnMountFromOrdinaryUnmount) {
    ASSERT_NO_FATAL_FAILURE(MountB());
    Fd held(open((a_ + "/" + file_).c_str(), O_RDONLY | O_CLOEXEC));
    ASSERT_GE(held.Get(), 0);
    errno = 0;
    ASSERT_EQ(-1, umount(a_.c_str()));
    ASSERT_EQ(EBUSY, errno);
    ASSERT_EQ(0, umount(b_.c_str())) << strerror(errno);
    b_mounted_ = false;
    held.Reset();
    // Retain a view for file cleanup, then independently retire the old view.
    ASSERT_NO_FATAL_FAILURE(MountB());
    ASSERT_NO_FATAL_FAILURE(UnmountA());
    ASSERT_NO_FATAL_FAILURE(ReadInitial(b_));
}

TEST_F(VirtiofsMultimount, LazyDetachedOpenFileSurvivesOtherViewUnmount) {
    ASSERT_NO_FATAL_FAILURE(MountB());
    Fd held(open((a_ + "/" + file_).c_str(), O_RDONLY | O_CLOEXEC));
    ASSERT_GE(held.Get(), 0);
    ASSERT_EQ(0, umount2(a_.c_str(), MNT_DETACH)) << strerror(errno);
    a_mounted_ = false;
    ASSERT_EQ(0, umount(b_.c_str())) << strerror(errno);
    b_mounted_ = false;
    char data[8] = {};
    ASSERT_EQ(8, read(held.Get(), data, sizeof(data))) << strerror(errno);
    EXPECT_EQ(0, memcmp(data, "initial", sizeof(data)));
    held.Reset();
    ASSERT_NO_FATAL_FAILURE(MountB());
    ASSERT_NO_FATAL_FAILURE(ReadInitial(b_));
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
