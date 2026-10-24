#include <gtest/gtest.h>

#include <fcntl.h>
#include <sched.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#include <cerrno>
#include <cstdlib>
#include <cstring>
#include <sstream>
#include <string>

namespace {

// Real virtiofs fixture: the export has .dunitest-automount-fixture, an
// "automount" bind with sample and nested/sample, a nested bind, and an
// "automount-sibling" bind. A /dev/fuse daemon cannot substitute for virtiofs:
// Linux negotiates FUSE_SUBMOUNTS only for an auto_submounts connection.
// Set DUNITEST_REQUIRE_VIRTIOFS_SUBMOUNT=1 to require this device fixture.
// DUNITEST_VIRTIOFS_EXPORT explicitly selects an existing ordinary export
// mount: nonrecursive binds isolate each test's mount topology while sharing
// the real virtiofs connection. Otherwise this suite mounts the hostshare tag.
class Fd {
 public:
    explicit Fd(int value = -1) : value_(value) {}
    ~Fd() { Reset(); }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int Get() const { return value_; }
    void Reset(int value = -1) {
        if (value_ >= 0) close(value_);
        value_ = value;
    }
 private:
    int value_;
};

std::string Setting(const char* name, const char* fallback) {
    const char* value = getenv(name);
    return value && *value ? value : fallback;
}

bool RelativeFixturePath(const std::string& path) {
    if (path.empty() || path.front() == '/' || path.back() == '/') return false;
    std::istringstream parts(path);
    std::string part;
    while (std::getline(parts, part, '/')) {
        if (part.empty() || part == "." || part == "..") return false;
    }
    return true;
}

bool MountedAt(const std::string& target) {
    Fd fd(open("/proc/self/mountinfo", O_RDONLY | O_CLOEXEC));
    if (fd.Get() < 0) return false;
    std::string contents;
    char chunk[4096];
    for (;;) {
        ssize_t count = read(fd.Get(), chunk, sizeof(chunk));
        if (count < 0 && errno == EINTR) continue;
        if (count <= 0) break;
        contents.append(chunk, static_cast<size_t>(count));
        if (contents.size() > 1024 * 1024) return false;
    }
    std::istringstream lines(contents);
    std::string line;
    while (std::getline(lines, line)) {
        std::istringstream fields(line);
        std::string id, parent, device, root, point;
        if (fields >> id >> parent >> device >> root >> point && point == target) return true;
    }
    return false;
}

class VirtiofsAutomount : public ::testing::Test {
 protected:
    std::string base_, view_, peer_;
    std::string child_, nested_, sibling_, file_;
    bool mounted_ = false;
    bool peer_mounted_ = false;

    void SetUp() override {
        alarm(60);
        const bool required = Setting("DUNITEST_REQUIRE_VIRTIOFS_SUBMOUNT", "0") == "1";
        child_ = Setting("DUNITEST_VIRTIOFS_SUBMOUNT_PATH", "automount");
        nested_ = Setting("DUNITEST_VIRTIOFS_NESTED_SUBMOUNT_PATH", "nested");
        sibling_ = Setting("DUNITEST_VIRTIOFS_SIBLING_SUBMOUNT_PATH", "automount-sibling");
        file_ = Setting("DUNITEST_VIRTIOFS_SUBMOUNT_FILE", "sample");
        ASSERT_TRUE(RelativeFixturePath(child_) && RelativeFixturePath(nested_)
                    && RelativeFixturePath(sibling_) && RelativeFixturePath(file_));
        char pattern[] = "/tmp/virtiofs_automount_XXXXXX";
        char* created = mkdtemp(pattern);
        ASSERT_NE(nullptr, created) << strerror(errno);
        base_ = created;
        view_ = base_ + "/view";
        peer_ = base_ + "/peer";
        ASSERT_EQ(0, mkdir(view_.c_str(), 0700)) << strerror(errno);
        ASSERT_EQ(0, mkdir(peer_.c_str(), 0700)) << strerror(errno);
        const std::string existing_export = Setting("DUNITEST_VIRTIOFS_EXPORT", "");
        ASSERT_TRUE(existing_export.empty() || existing_export.front() == '/');
        const int mounted = existing_export.empty()
            ? mount("hostshare", view_.c_str(), "virtiofs", 0, nullptr)
            : mount(existing_export.c_str(), view_.c_str(), nullptr, MS_BIND, nullptr);
        if (mounted != 0) {
            const int error = errno;
            if (!required && (error == ENODEV || error == ENOENT || error == EINVAL)) {
                GTEST_SKIP() << "virtiofs automount fixture unavailable: " << strerror(error);
            }
            FAIL() << "virtiofs mount failed: " << strerror(error);
        }
        mounted_ = true;
        Fd marker(open((view_ + "/.dunitest-automount-fixture").c_str(), O_RDONLY | O_CLOEXEC));
        if (marker.Get() < 0 && !required) GTEST_SKIP() << "real host submount fixture not configured";
        ASSERT_GE(marker.Get(), 0) << "required host submount fixture missing: " << strerror(errno);
    }

    void TearDown() override {
        alarm(0);
        if (peer_mounted_) {
            if (umount(peer_.c_str()) != 0) {
                ADD_FAILURE() << "peer ordinary cleanup failed: " << strerror(errno);
                umount2(peer_.c_str(), MNT_DETACH);
            }
        }
        if (mounted_) {
            if (umount(view_.c_str()) != 0) {
                ADD_FAILURE() << "view ordinary cleanup failed: " << strerror(errno);
                umount2(view_.c_str(), MNT_DETACH);
            }
        }
        if (!peer_.empty()) rmdir(peer_.c_str());
        if (!view_.empty()) rmdir(view_.c_str());
        if (!base_.empty()) rmdir(base_.c_str());
    }

    std::string Child() const { return view_ + "/" + child_; }
    std::string Sample() const { return Child() + "/" + file_; }

    void Trigger(const std::string& path) {
        Fd fd(open(path.c_str(), O_RDONLY | O_CLOEXEC));
        ASSERT_GE(fd.Get(), 0) << path << ": " << strerror(errno);
        char byte;
        ASSERT_EQ(1, read(fd.Get(), &byte, 1)) << strerror(errno);
    }

    void Busy() {
        errno = 0;
        ASSERT_EQ(-1, umount(view_.c_str()));
        ASSERT_EQ(EBUSY, errno) << strerror(errno);
        ASSERT_TRUE(MountedAt(view_));
    }

    void Finish() {
        ASSERT_EQ(0, umount(view_.c_str())) << strerror(errno);
        mounted_ = false;
        EXPECT_FALSE(MountedAt(view_));
    }
};

TEST_F(VirtiofsAutomount, IdleChildIsReclaimedByOrdinaryParentUnmount) {
    ASSERT_NO_FATAL_FAILURE(Trigger(Sample()));
    ASSERT_TRUE(MountedAt(Child()));
    ASSERT_NO_FATAL_FAILURE(Finish());
    EXPECT_FALSE(MountedAt(Child()));
}

TEST_F(VirtiofsAutomount, NestedIdleChildrenAreReclaimedBottomUp) {
    ASSERT_NO_FATAL_FAILURE(Trigger(Child() + "/" + nested_ + "/" + file_));
    ASSERT_TRUE(MountedAt(Child()));
    ASSERT_TRUE(MountedAt(Child() + "/" + nested_));
    ASSERT_NO_FATAL_FAILURE(Finish());
    EXPECT_FALSE(MountedAt(Child() + "/" + nested_));
}

TEST_F(VirtiofsAutomount, OpenFileKeepsChildBusyUntilClose) {
    Fd fd(open(Sample().c_str(), O_RDONLY | O_CLOEXEC));
    ASSERT_GE(fd.Get(), 0) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(Busy());
    EXPECT_TRUE(MountedAt(Child()));
    fd.Reset();
    ASSERT_NO_FATAL_FAILURE(Finish());
}

TEST_F(VirtiofsAutomount, OPathKeepsChildBusyUntilClose) {
    Fd fd(open(Sample().c_str(), O_PATH | O_CLOEXEC));
    ASSERT_GE(fd.Get(), 0) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(Busy());
    EXPECT_TRUE(MountedAt(Child()));
    fd.Reset();
    ASSERT_NO_FATAL_FAILURE(Finish());
}

TEST_F(VirtiofsAutomount, CwdKeepsChildBusyUntilChdir) {
    Fd old_cwd(open(".", O_PATH | O_CLOEXEC));
    ASSERT_GE(old_cwd.Get(), 0) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(Trigger(Sample()));
    ASSERT_EQ(0, chdir(Child().c_str())) << strerror(errno);
    // Restore cwd even if an expectation fails, so no test contaminates another.
    const int result = umount(view_.c_str());
    const int error = errno;
    EXPECT_EQ(0, fchdir(old_cwd.Get())) << strerror(errno);
    ASSERT_EQ(-1, result);
    ASSERT_EQ(EBUSY, error) << strerror(error);
    EXPECT_TRUE(MountedAt(Child()));
    ASSERT_NO_FATAL_FAILURE(Finish());
}

TEST_F(VirtiofsAutomount, MappingKeepsChildBusyAfterFileClose) {
    Fd fd(open(Sample().c_str(), O_RDONLY | O_CLOEXEC));
    ASSERT_GE(fd.Get(), 0) << strerror(errno);
    struct stat info {};
    ASSERT_EQ(0, fstat(fd.Get(), &info));
    ASSERT_GE(info.st_size, 4096);
    void* mapping = mmap(nullptr, 4096, PROT_READ, MAP_PRIVATE, fd.Get(), 0);
    ASSERT_NE(MAP_FAILED, mapping) << strerror(errno);
    fd.Reset();
    const int result = umount(view_.c_str());
    const int error = errno;
    EXPECT_EQ(0, munmap(mapping, 4096)) << strerror(errno);
    ASSERT_EQ(-1, result);
    ASSERT_EQ(EBUSY, error) << strerror(error);
    ASSERT_NO_FATAL_FAILURE(Finish());
}

TEST_F(VirtiofsAutomount, IdleSiblingIsReclaimedEvenWhenParentRemainsBusy) {
    Fd busy(open(Sample().c_str(), O_RDONLY | O_CLOEXEC));
    ASSERT_GE(busy.Get(), 0) << strerror(errno);
    const std::string sibling = view_ + "/" + sibling_;
    ASSERT_NO_FATAL_FAILURE(Trigger(sibling + "/" + file_));
    ASSERT_TRUE(MountedAt(sibling));
    ASSERT_NO_FATAL_FAILURE(Busy());
    EXPECT_TRUE(MountedAt(Child()));
    EXPECT_FALSE(MountedAt(sibling));
    busy.Reset();
    ASSERT_NO_FATAL_FAILURE(Finish());
}

TEST_F(VirtiofsAutomount, OrdinaryChildCannotBeReclaimed) {
    const std::string ordinary = view_ + "/results";
    ASSERT_EQ(0, mount("none", ordinary.c_str(), "tmpfs", 0, nullptr)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(Busy());
    EXPECT_TRUE(MountedAt(ordinary));
    ASSERT_EQ(0, umount(ordinary.c_str())) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(Finish());
}

TEST_F(VirtiofsAutomount, UserspaceSubmountFlagDoesNotGrantAutomountOrigin) {
    const std::string ordinary = view_ + "/results";
    ASSERT_EQ(0, mount("none", ordinary.c_str(), "tmpfs", 1UL << 26, nullptr)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(Busy());
    EXPECT_TRUE(MountedAt(ordinary));
    ASSERT_EQ(0, umount(ordinary.c_str())) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(Finish());
}

TEST_F(VirtiofsAutomount, OrdinaryChildProtectsItsIdleAutomountDescendant) {
    const std::string ordinary = view_ + "/results";
    // A manual bind is an ordinary child, unlike the automatic descendants
    // discovered below it. It does not require a second device connection.
    ASSERT_EQ(0, mount(view_.c_str(), ordinary.c_str(), nullptr, MS_BIND, nullptr)) << strerror(errno);
    const std::string descendant = ordinary + "/" + child_;
    ASSERT_NO_FATAL_FAILURE(Trigger(descendant + "/" + file_));
    ASSERT_TRUE(MountedAt(descendant));
    ASSERT_NO_FATAL_FAILURE(Busy());
    EXPECT_TRUE(MountedAt(ordinary));
    EXPECT_TRUE(MountedAt(descendant));
    ASSERT_EQ(0, umount(ordinary.c_str())) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(Finish());
}

TEST_F(VirtiofsAutomount, BindCloneRetainsAutomountOrigin) {
    ASSERT_NO_FATAL_FAILURE(Trigger(Sample()));
    const std::string alias = view_ + "/results";
    ASSERT_EQ(0, mount(Child().c_str(), alias.c_str(), nullptr, MS_BIND, nullptr)) << strerror(errno);
    ASSERT_TRUE(MountedAt(alias));
    ASSERT_NO_FATAL_FAILURE(Finish());
    EXPECT_FALSE(MountedAt(alias));
}

TEST_F(VirtiofsAutomount, NamespaceCloneRetainsAutomountOrigin) {
    ASSERT_NO_FATAL_FAILURE(Trigger(Sample()));
    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        if (unshare(CLONE_NEWNS) != 0) _exit(10);
        if (umount(view_.c_str()) != 0) _exit(11);
        _exit(0);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(0, WEXITSTATUS(status));
    EXPECT_TRUE(MountedAt(Child()));
    ASSERT_NO_FATAL_FAILURE(Finish());
}

TEST_F(VirtiofsAutomount, LockedParentDoesNotReclaimIdleChild) {
    ASSERT_NO_FATAL_FAILURE(Trigger(Sample()));
    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        if (unshare(CLONE_NEWUSER | CLONE_NEWNS) != 0) _exit(10);
        errno = 0;
        if (umount(view_.c_str()) != -1 || errno != EINVAL) _exit(11);
        if (!MountedAt(Child())) _exit(12);
        _exit(0);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(0, WEXITSTATUS(status));
    EXPECT_TRUE(MountedAt(Child()));
    ASSERT_NO_FATAL_FAILURE(Finish());
}

TEST_F(VirtiofsAutomount, BusyPropagationPeerPreventsReclaim) {
    ASSERT_EQ(0, mount(nullptr, view_.c_str(), nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(view_.c_str(), peer_.c_str(), nullptr, MS_BIND, nullptr)) << strerror(errno);
    peer_mounted_ = true;
    ASSERT_NO_FATAL_FAILURE(Trigger(Sample()));
    const std::string peer_child = peer_ + "/" + child_;
    ASSERT_TRUE(MountedAt(peer_child));
    Fd peer_fd(open((peer_child + "/" + file_).c_str(), O_RDONLY | O_CLOEXEC));
    ASSERT_GE(peer_fd.Get(), 0) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(Busy());
    EXPECT_TRUE(MountedAt(Child()));
    EXPECT_TRUE(MountedAt(peer_child));
    peer_fd.Reset();
    ASSERT_NO_FATAL_FAILURE(Finish());
    EXPECT_FALSE(MountedAt(peer_child));
    ASSERT_EQ(0, umount(peer_.c_str())) << strerror(errno);
    peer_mounted_ = false;
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
