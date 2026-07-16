#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef CLONE_NEWNS
#define CLONE_NEWNS 0x00020000
#endif

#ifndef CLONE_NEWUSER
#define CLONE_NEWUSER 0x10000000
#endif

#ifndef MS_REC
#define MS_REC 16384
#endif

#ifndef MS_SILENT
#define MS_SILENT 32768
#endif

namespace {

int ensure_dir(const char* path) {
    struct stat st = {};
    if (stat(path, &st) == 0) {
        return S_ISDIR(st.st_mode) ? 0 : -1;
    }
    return mkdir(path, 0755);
}

bool path_exists(const char* path) {
    struct stat st = {};
    return stat(path, &st) == 0;
}

int create_marker(const char* mount_point, const char* name) {
    char path[256] = {};
    snprintf(path, sizeof(path), "%s/%s", mount_point, name);

    int fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0) {
        return -1;
    }
    const int ret = write(fd, "x", 1);
    const int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    return ret == 1 ? 0 : -1;
}

bool marker_exists(const char* mount_point, const char* name) {
    char path[256] = {};
    snprintf(path, sizeof(path), "%s/%s", mount_point, name);
    return path_exists(path);
}

void best_effort_umount(const char* path) {
    if (umount2(path, MNT_DETACH) != 0 && errno != EINVAL && errno != ENOENT) {
        ADD_FAILURE() << "umount failed for " << path << ": errno=" << errno << " ("
                      << strerror(errno) << ")";
    }
}

void best_effort_rmdir(const char* path) {
    if (rmdir(path) != 0 && errno != ENOENT && errno != ENOTEMPTY) {
        ADD_FAILURE() << "rmdir failed for " << path << ": errno=" << errno << " ("
                      << strerror(errno) << ")";
    }
}

void cleanup_path(const char* path) {
    umount(path);
    rmdir(path);
}

int shared_group_id(const char* mount_point) {
    FILE* fp = fopen("/proc/self/mountinfo", "r");
    if (fp == nullptr) {
        return -1;
    }

    int result = -1;
    char line[1024] = {};
    while (fgets(line, sizeof(line), fp) != nullptr) {
        char parsed_mount_point[256] = {};
        if (sscanf(line, "%*s %*s %*s %*s %255s", parsed_mount_point) != 1 ||
            strcmp(parsed_mount_point, mount_point) != 0) {
            continue;
        }

        char* optional_end = strstr(line, " - ");
        char* shared = strstr(line, " shared:");
        if (shared != nullptr && optional_end != nullptr && shared < optional_end &&
            sscanf(shared, " shared:%d", &result) == 1) {
            break;
        }
        result = -1;
        break;
    }
    fclose(fp);
    return result;
}

bool mount_source_at(const char* mount_point, const char* expected_source) {
    FILE* fp = fopen("/proc/self/mountinfo", "r");
    if (fp == nullptr) {
        return false;
    }

    bool found = false;
    char line[1024] = {};
    while (fgets(line, sizeof(line), fp) != nullptr) {
        char parsed_mount_point[256] = {};
        if (sscanf(line, "%*s %*s %*s %*s %255s", parsed_mount_point) != 1 ||
            strcmp(parsed_mount_point, mount_point) != 0) {
            continue;
        }

        char* separator = strstr(line, " - ");
        char source[256] = {};
        found = separator != nullptr &&
                sscanf(separator + 3, "%*s %255s", source) == 1 &&
                strcmp(source, expected_source) == 0;
        break;
    }
    fclose(fp);
    return found;
}

bool mount_has_option(const char* mount_point, const char* expected_option) {
    FILE* fp = fopen("/proc/self/mountinfo", "r");
    if (fp == nullptr) {
        return false;
    }

    bool found = false;
    char line[2048] = {};
    while (fgets(line, sizeof(line), fp) != nullptr) {
        char parsed_mount_point[256] = {};
        char options[512] = {};
        if (sscanf(line, "%*s %*s %*s %*s %255s %511s", parsed_mount_point, options) != 2 ||
            strcmp(parsed_mount_point, mount_point) != 0) {
            continue;
        }
        char* save = nullptr;
        for (char* option = strtok_r(options, ",", &save); option != nullptr;
             option = strtok_r(nullptr, ",", &save)) {
            if (strcmp(option, expected_option) == 0) {
                found = true;
                break;
            }
        }
        break;
    }
    fclose(fp);
    return found;
}

struct PropagationTags {
    int shared = -1;
    int master = -1;
    int propagate_from = -1;
    bool unbindable = false;
};

bool parse_mountinfo_tags(char* line, const char* mount_point, PropagationTags* tags) {
    char* save = nullptr;
    char* token = strtok_r(line, " ", &save);
    for (int field = 1; field <= 6; ++field) {
        if (token == nullptr) {
            return false;
        }
        if (field == 5 && strcmp(token, mount_point) != 0) {
            return false;
        }
        token = strtok_r(nullptr, " ", &save);
    }

    *tags = {};
    while (token != nullptr && strcmp(token, "-") != 0) {
        if (sscanf(token, "shared:%d", &tags->shared) == 1 ||
            sscanf(token, "master:%d", &tags->master) == 1 ||
            sscanf(token, "propagate_from:%d", &tags->propagate_from) == 1) {
            token = strtok_r(nullptr, " ", &save);
            continue;
        }
        if (strcmp(token, "unbindable") == 0) {
            tags->unbindable = true;
        }
        token = strtok_r(nullptr, " ", &save);
    }
    return token != nullptr;
}

bool read_propagation_snapshot(const char* const* mount_points, size_t count,
                               PropagationTags* tags) {
    FILE* fp = fopen("/proc/self/mountinfo", "r");
    if (fp == nullptr) {
        return false;
    }

    bool found[8] = {};
    if (count > sizeof(found) / sizeof(found[0])) {
        fclose(fp);
        return false;
    }
    char line[2048] = {};
    while (fgets(line, sizeof(line), fp) != nullptr) {
        for (size_t i = 0; i < count; ++i) {
            if (found[i]) {
                continue;
            }
            char copy[sizeof(line)] = {};
            const size_t line_len = strnlen(line, sizeof(copy) - 1);
            memcpy(copy, line, line_len);
            copy[line_len] = '\0';
            if (parse_mountinfo_tags(copy, mount_points[i], &tags[i])) {
                found[i] = true;
            }
        }
    }
    fclose(fp);
    for (size_t i = 0; i < count; ++i) {
        if (!found[i]) {
            return false;
        }
    }
    return true;
}

bool snapshot_is_uniform(const PropagationTags* tags, size_t count) {
    const bool shared = tags[0].shared > 0;
    for (size_t i = 0; i < count; ++i) {
        if ((tags[i].shared > 0) != shared || tags[i].master >= 0 ||
            tags[i].propagate_from >= 0 || tags[i].unbindable) {
            return false;
        }
    }
    return true;
}

bool read_exact(int fd, void* buffer, size_t length) {
    auto* bytes = static_cast<char*>(buffer);
    size_t offset = 0;
    while (offset < length) {
        const ssize_t result = read(fd, bytes + offset, length - offset);
        if (result > 0) {
            offset += static_cast<size_t>(result);
        } else if (result < 0 && errno == EINTR) {
            continue;
        } else {
            return false;
        }
    }
    return true;
}

bool write_exact(int fd, const void* buffer, size_t length) {
    const auto* bytes = static_cast<const char*>(buffer);
    size_t offset = 0;
    while (offset < length) {
        const ssize_t result = write(fd, bytes + offset, length - offset);
        if (result > 0) {
            offset += static_cast<size_t>(result);
        } else if (result < 0 && errno == EINTR) {
            continue;
        } else {
            return false;
        }
    }
    return true;
}

class ChildProcessGuard {
public:
    explicit ChildProcessGuard(pid_t pid) : pid_(pid) {}

    ~ChildProcessGuard() {
        if (pid_ <= 0) {
            return;
        }
        kill(pid_, SIGKILL);
        while (waitpid(pid_, nullptr, 0) < 0 && errno == EINTR) {
        }
    }

    pid_t wait(int* status) {
        pid_t result;
        do {
            result = waitpid(pid_, status, 0);
        } while (result < 0 && errno == EINTR);
        if (result == pid_) {
            pid_ = -1;
        }
        return result;
    }

private:
    pid_t pid_;
};

void terminate_children(pid_t* children, size_t count) {
    for (size_t i = 0; i < count; ++i) {
        if (children[i] > 0) {
            kill(children[i], SIGKILL);
        }
    }
    for (size_t i = 0; i < count; ++i) {
        if (children[i] <= 0) {
            continue;
        }
        while (waitpid(children[i], nullptr, 0) < 0 && errno == EINTR) {
        }
        children[i] = -1;
    }
}

bool wait_children_until(pid_t* children, size_t count, int timeout_seconds) {
    timespec deadline = {};
    if (clock_gettime(CLOCK_MONOTONIC, &deadline) != 0) {
        terminate_children(children, count);
        return false;
    }
    deadline.tv_sec += timeout_seconds;
    size_t remaining = count;
    bool ok = true;
    while (remaining != 0) {
        for (size_t i = 0; i < count; ++i) {
            if (children[i] <= 0) {
                continue;
            }
            int status = 0;
            const pid_t result = waitpid(children[i], &status, WNOHANG);
            if (result == children[i]) {
                ok = ok && WIFEXITED(status) && WEXITSTATUS(status) == 0;
                children[i] = -1;
                --remaining;
            } else if (result < 0 && errno != EINTR) {
                ok = false;
                break;
            }
        }
        if (!ok) {
            break;
        }
        if (remaining == 0) {
            break;
        }
        timespec now = {};
        if (clock_gettime(CLOCK_MONOTONIC, &now) != 0 ||
            now.tv_sec > deadline.tv_sec ||
            (now.tv_sec == deadline.tv_sec && now.tv_nsec >= deadline.tv_nsec)) {
            ok = false;
            break;
        }
        sched_yield();
    }
    if (remaining != 0 || !ok) {
        terminate_children(children, count);
    }
    return ok && remaining == 0;
}

class MountPropagationTest : public ::testing::Test {
protected:
    char root_[128] = {};

    void SetUp() override {
        ASSERT_EQ(0, ensure_dir("/tmp")) << strerror(errno);
        snprintf(root_, sizeof(root_), "/tmp/mount_propagation_%d", getpid());
        ASSERT_EQ(0, ensure_dir(root_)) << strerror(errno);

        if (unshare(CLONE_NEWNS) != 0) {
            GTEST_SKIP() << "unshare(CLONE_NEWNS): " << strerror(errno);
        }
        ASSERT_EQ(0, mount(nullptr, "/", nullptr, MS_REC | MS_PRIVATE, nullptr))
            << strerror(errno);
    }

    void TearDown() override {
        char path[192] = {};
        const char* suffixes[] = {
            "/target_b/bind/host", "/target_a/bind/host", "/source/host",  "/master/host",
            "/src/host",           "/target_b/bind",      "/target_a/bind", "/target_b",
            "/target_a",           "/source",             "/master",       "/src",
            "/slave_a/local",      "/slave_b/local",      "/slave/local",  "/base/host",
            "/slave_a/host",       "/slave_b/host",       "/slave/host",   "/slave_b",
            "/slave_a",            "/slave",              "/base/child/grandchild",
            "/base/child",         "/base/dynamic",       "/base",
        };

        for (const char* suffix : suffixes) {
            snprintf(path, sizeof(path), "%s%s", root_, suffix);
            cleanup_path(path);
        }
        best_effort_rmdir(root_);
    }
};

}  // namespace

TEST_F(MountPropagationTest, PropagationFlagsAreStrictlyValidated) {
    char base[160] = {};
    snprintf(base, sizeof(base), "%s/base", root_);

    ASSERT_EQ(0, ensure_dir(base)) << strerror(errno);
    ASSERT_EQ(0, mount("", base, "ramfs", 0, nullptr)) << strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, mount(nullptr, base, nullptr, MS_SHARED | MS_PRIVATE, nullptr));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(-1, shared_group_id(base));

    errno = 0;
    EXPECT_EQ(-1, mount(nullptr, base, nullptr, MS_SHARED | 0x200, nullptr));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(-1, shared_group_id(base));

    errno = 0;
    EXPECT_EQ(-1, mount(nullptr, base, nullptr, MS_SHARED | MS_NODEV, nullptr));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(-1, shared_group_id(base));

    ASSERT_EQ(0, mount(nullptr, base, nullptr, MS_SHARED | MS_REC | MS_SILENT, nullptr))
        << strerror(errno);
    EXPECT_GT(shared_group_id(base), 0);

    best_effort_umount(base);
    best_effort_rmdir(base);
}

TEST_F(MountPropagationTest, GroupIdIsReusedOnlyAfterLastPeerLeaves) {
    char base[160] = {};
    char slave[160] = {};
    char master[160] = {};
    snprintf(base, sizeof(base), "%s/base", root_);
    snprintf(slave, sizeof(slave), "%s/slave", root_);
    snprintf(master, sizeof(master), "%s/master", root_);

    ASSERT_EQ(0, ensure_dir(base)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(slave)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(master)) << strerror(errno);
    ASSERT_EQ(0, mount("", base, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, base, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    const int original_group = shared_group_id(base);
    ASSERT_GT(original_group, 0);

    ASSERT_EQ(0, mount(base, slave, nullptr, MS_BIND, nullptr)) << strerror(errno);
    ASSERT_EQ(original_group, shared_group_id(slave));
    ASSERT_EQ(0, umount(base)) << strerror(errno);

    ASSERT_EQ(0, mount("", master, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, master, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    const int live_peer_group = shared_group_id(master);
    ASSERT_GT(live_peer_group, 0);
    EXPECT_NE(original_group, live_peer_group);

    ASSERT_EQ(0, umount(slave)) << strerror(errno);
    ASSERT_EQ(0, mount("", base, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, base, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    EXPECT_EQ(original_group, shared_group_id(base));

    best_effort_umount(base);
    best_effort_umount(master);
    best_effort_rmdir(base);
    best_effort_rmdir(slave);
    best_effort_rmdir(master);
}

TEST_F(MountPropagationTest, PropagatedMountPreservesSourceMetadata) {
    char base[160] = {};
    char peer[160] = {};
    char base_host[192] = {};
    char peer_host[192] = {};
    snprintf(base, sizeof(base), "%s/base", root_);
    snprintf(peer, sizeof(peer), "%s/slave", root_);
    snprintf(base_host, sizeof(base_host), "%s/host", base);
    snprintf(peer_host, sizeof(peer_host), "%s/host", peer);

    ASSERT_EQ(0, ensure_dir(base)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(peer)) << strerror(errno);
    ASSERT_EQ(0, mount("", base, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, base, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(base, peer, nullptr, MS_BIND, nullptr)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(base_host)) << strerror(errno);

    ASSERT_EQ(0, mount("issue1978-source", base_host, "ramfs", 0, nullptr)) << strerror(errno);
    EXPECT_TRUE(mount_source_at(base_host, "issue1978-source"));
    EXPECT_TRUE(mount_source_at(peer_host, "issue1978-source"));

    best_effort_umount(base_host);
    best_effort_rmdir(base_host);
    best_effort_umount(peer);
    best_effort_rmdir(peer);
    best_effort_umount(base);
    best_effort_rmdir(base);
}

TEST_F(MountPropagationTest, CrossUserNamespaceCanMakeLockedRootPrivate) {
    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        if (unshare(CLONE_NEWUSER | CLONE_NEWNS) != 0) {
            _exit(1);
        }
        if (mount(nullptr, "/", nullptr, MS_REC | MS_PRIVATE, nullptr) != 0) {
            _exit(100 + errno);
        }
        _exit(0);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST_F(MountPropagationTest, CrossUserCopyIsOneWayAndKeepsAttributeLocksAcrossCopy) {
    char base[160] = {};
    char host[192] = {};
    char local[192] = {};
    snprintf(base, sizeof(base), "%s/base", root_);
    snprintf(host, sizeof(host), "%s/host", base);
    snprintf(local, sizeof(local), "%s/local", base);

    ASSERT_EQ(0, ensure_dir(base)) << strerror(errno);
    ASSERT_EQ(0, mount("", base, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(host)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(local)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, base, nullptr,
                       MS_REMOUNT | MS_BIND | MS_NOSUID | MS_NODEV | MS_NOEXEC, nullptr))
        << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, base, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    const int source_group = shared_group_id(base);
    ASSERT_GT(source_group, 0);

    int parent_to_child[2] = {-1, -1};
    int child_to_parent[2] = {-1, -1};
    ASSERT_EQ(0, pipe(parent_to_child)) << strerror(errno);
    ASSERT_EQ(0, pipe(child_to_parent)) << strerror(errno);

    struct ChildReport {
        PropagationTags initial_tags;
        PropagationTags nested_tags;
        int initial_remount_errno;
        int add_readonly_errno;
        int readonly_after_add;
        int remove_readonly_errno;
        int readonly_after_remove;
        int atime_remount_errno;
        int nested_unshare_errno;
        int nested_remount_errno;
        int ordinary_remount_errno;
        int saw_parent_mount;
        int local_mount_errno;
        int saw_local_mount;
    };

    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    ChildProcessGuard child_guard(child);
    if (child == 0) {
        close(parent_to_child[1]);
        close(child_to_parent[0]);
        ChildReport report = {};
        report.initial_tags = {};
        report.nested_tags = {};

        if (unshare(CLONE_NEWUSER | CLONE_NEWNS) != 0) {
            _exit(1);
        }
        const char* points[] = {base};
        if (!read_propagation_snapshot(points, 1, &report.initial_tags)) {
            _exit(2);
        }
        errno = 0;
        if (mount(nullptr, base, nullptr, MS_REMOUNT | MS_BIND, nullptr) == 0) {
            report.initial_remount_errno = 0;
        } else {
            report.initial_remount_errno = errno;
        }
        errno = 0;
        if (mount(nullptr, base, nullptr,
                  MS_REMOUNT | MS_BIND | MS_RDONLY | MS_NOSUID | MS_NODEV | MS_NOEXEC,
                  nullptr) != 0) {
            report.add_readonly_errno = errno;
        }
        report.readonly_after_add = mount_has_option(base, "ro") ? 1 : 0;
        errno = 0;
        if (mount(nullptr, base, nullptr,
                  MS_REMOUNT | MS_BIND | MS_NOSUID | MS_NODEV | MS_NOEXEC, nullptr) != 0) {
            report.remove_readonly_errno = errno;
        }
        report.readonly_after_remove = mount_has_option(base, "ro") ? 1 : 0;
        errno = 0;
        if (mount(nullptr, base, nullptr,
                  MS_REMOUNT | MS_BIND | MS_NOATIME | MS_NOSUID | MS_NODEV | MS_NOEXEC,
                  nullptr) == 0) {
            report.atime_remount_errno = 0;
        } else {
            report.atime_remount_errno = errno;
        }

        errno = 0;
        if (unshare(CLONE_NEWNS) != 0) {
            report.nested_unshare_errno = errno;
        } else {
            if (!read_propagation_snapshot(points, 1, &report.nested_tags)) {
                _exit(3);
            }
            errno = 0;
            if (mount(nullptr, base, nullptr, MS_REMOUNT | MS_BIND, nullptr) == 0) {
                report.nested_remount_errno = 0;
            } else {
                report.nested_remount_errno = errno;
            }
            errno = 0;
            if (mount(nullptr, base, nullptr, MS_REMOUNT, nullptr) == 0) {
                report.ordinary_remount_errno = 0;
            } else {
                report.ordinary_remount_errno = errno;
            }
        }

        const char ready = 'R';
        if (!write_exact(child_to_parent[1], &ready, sizeof(ready))) {
            _exit(4);
        }
        char release = 0;
        if (!read_exact(parent_to_child[0], &release, sizeof(release))) {
            _exit(5);
        }
        report.saw_parent_mount = marker_exists(host, "from_parent") ? 1 : 0;
        errno = 0;
        if (mount("issue2103-child", local, "ramfs", 0, nullptr) != 0) {
            report.local_mount_errno = errno;
        }
        report.saw_local_mount = mount_source_at(local, "issue2103-child") ? 1 : 0;
        if (!write_exact(child_to_parent[1], &report, sizeof(report))) {
            _exit(6);
        }
        _exit(0);
    }

    close(parent_to_child[0]);
    close(child_to_parent[1]);
    char ready = 0;
    ASSERT_TRUE(read_exact(child_to_parent[0], &ready, sizeof(ready)));
    ASSERT_EQ('R', ready);
    ASSERT_EQ(0, mount("", host, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, create_marker(host, "from_parent")) << strerror(errno);
    const char release = 'G';
    ASSERT_TRUE(write_exact(parent_to_child[1], &release, sizeof(release)));

    ChildReport report = {};
    ASSERT_TRUE(read_exact(child_to_parent[0], &report, sizeof(report)));
    int status = 0;
    ASSERT_EQ(child, child_guard.wait(&status)) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(0, WEXITSTATUS(status));

    EXPECT_EQ(-1, report.initial_tags.shared);
    EXPECT_EQ(source_group, report.initial_tags.master);
    EXPECT_EQ(EPERM, report.initial_remount_errno);
    EXPECT_EQ(0, report.add_readonly_errno);
    EXPECT_EQ(1, report.readonly_after_add);
    EXPECT_EQ(0, report.remove_readonly_errno);
    EXPECT_EQ(0, report.readonly_after_remove);
    EXPECT_EQ(EPERM, report.atime_remount_errno);
    EXPECT_EQ(0, report.nested_unshare_errno);
    EXPECT_EQ(-1, report.nested_tags.shared);
    EXPECT_EQ(source_group, report.nested_tags.master);
    EXPECT_EQ(EPERM, report.nested_remount_errno);
    EXPECT_EQ(EPERM, report.ordinary_remount_errno);
    EXPECT_EQ(1, report.saw_parent_mount);
    EXPECT_EQ(0, report.local_mount_errno);
    EXPECT_EQ(1, report.saw_local_mount);
    EXPECT_FALSE(mount_source_at(local, "issue2103-child"));

    close(parent_to_child[1]);
    close(child_to_parent[0]);
    best_effort_umount(host);
    best_effort_umount(base);
    best_effort_rmdir(host);
    best_effort_rmdir(local);
    best_effort_rmdir(base);
}

TEST_F(MountPropagationTest, SameUserNamespaceCopyRemainsInSharedPeerGroup) {
    char base[160] = {};
    char local[192] = {};
    snprintf(base, sizeof(base), "%s/base", root_);
    snprintf(local, sizeof(local), "%s/local", base);

    ASSERT_EQ(0, ensure_dir(base)) << strerror(errno);
    ASSERT_EQ(0, mount("", base, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(local)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, base, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    const int source_group = shared_group_id(base);
    ASSERT_GT(source_group, 0);

    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        if (unshare(CLONE_NEWNS) != 0) {
            _exit(1);
        }
        const char* points[] = {base};
        PropagationTags tags = {};
        if (!read_propagation_snapshot(points, 1, &tags) || tags.shared != source_group ||
            tags.master >= 0) {
            _exit(2);
        }
        if (mount(nullptr, base, nullptr, MS_REMOUNT | MS_BIND | MS_NOATIME, nullptr) != 0 ||
            !mount_has_option(base, "noatime")) {
            _exit(3);
        }
        if (mount("", local, "ramfs", 0, nullptr) != 0 ||
            create_marker(local, "same_user") != 0) {
            _exit(4);
        }
        _exit(0);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(0, WEXITSTATUS(status));
    EXPECT_TRUE(marker_exists(local, "same_user"));

    best_effort_umount(local);
    best_effort_umount(base);
    best_effort_rmdir(local);
    best_effort_rmdir(base);
}

TEST_F(MountPropagationTest, CrossUserCopyCannotReconfigureSourceSuperblock) {
    char base[160] = {};
    snprintf(base, sizeof(base), "%s/base", root_);
    ASSERT_EQ(0, ensure_dir(base)) << strerror(errno);
    ASSERT_EQ(0, mount("", base, "ramfs", 0, nullptr)) << strerror(errno);

    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        if (unshare(CLONE_NEWUSER | CLONE_NEWNS) != 0) {
            _exit(1);
        }
        errno = 0;
        if (mount(nullptr, base, nullptr, MS_REMOUNT | MS_RDONLY, nullptr) == 0 ||
            errno != EPERM) {
            _exit(2);
        }
        _exit(0);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(0, WEXITSTATUS(status));
    ASSERT_EQ(0, mount(nullptr, base, nullptr, MS_REMOUNT | MS_RDONLY, nullptr))
        << strerror(errno);
    EXPECT_TRUE(mount_has_option(base, "ro"));
    ASSERT_EQ(0, mount(nullptr, base, nullptr, MS_REMOUNT, nullptr)) << strerror(errno);
    EXPECT_FALSE(mount_has_option(base, "ro"));

    best_effort_umount(base);
    best_effort_rmdir(base);
}

TEST_F(MountPropagationTest, UserAndMountNamespaceCreationOrderMatchesLinux) {
    char base[160] = {};
    snprintf(base, sizeof(base), "%s/base", root_);
    ASSERT_EQ(0, ensure_dir(base)) << strerror(errno);
    ASSERT_EQ(0, mount("", base, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, base, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    const int source_group = shared_group_id(base);
    ASSERT_GT(source_group, 0);

    pid_t sequential = fork();
    ASSERT_GE(sequential, 0) << strerror(errno);
    if (sequential == 0) {
        if (unshare(CLONE_NEWUSER) != 0 || unshare(CLONE_NEWNS) != 0) {
            _exit(1);
        }
        const char* points[] = {base};
        PropagationTags tags = {};
        if (!read_propagation_snapshot(points, 1, &tags) || tags.shared >= 0 ||
            tags.master != source_group) {
            _exit(2);
        }
        _exit(0);
    }

    int status = 0;
    ASSERT_EQ(sequential, waitpid(sequential, &status, 0)) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));

    pid_t reverse = fork();
    ASSERT_GE(reverse, 0) << strerror(errno);
    if (reverse == 0) {
        if (unshare(CLONE_NEWNS) != 0 || unshare(CLONE_NEWUSER) != 0) {
            _exit(1);
        }
        errno = 0;
        if (mount(nullptr, base, nullptr, MS_PRIVATE, nullptr) == 0 || errno != EPERM) {
            _exit(2);
        }
        if (unshare(CLONE_NEWNS) != 0) {
            _exit(3);
        }
        const char* points[] = {base};
        PropagationTags tags = {};
        if (!read_propagation_snapshot(points, 1, &tags) || tags.shared >= 0 ||
            tags.master != source_group) {
            _exit(4);
        }
        if (mount(nullptr, base, nullptr, MS_PRIVATE, nullptr) != 0) {
            _exit(5);
        }
        _exit(0);
    }

    ASSERT_EQ(reverse, waitpid(reverse, &status, 0)) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));

    best_effort_umount(base);
    best_effort_rmdir(base);
}

TEST_F(MountPropagationTest, CrossUserSharedSlaveUsesSourceAsImmediateMaster) {
    char base[160] = {};
    char slave[160] = {};
    snprintf(base, sizeof(base), "%s/base", root_);
    snprintf(slave, sizeof(slave), "%s/slave", root_);

    ASSERT_EQ(0, ensure_dir(base)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(slave)) << strerror(errno);
    ASSERT_EQ(0, mount("", base, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, base, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(base, slave, nullptr, MS_BIND, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, slave, nullptr, MS_SLAVE, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, slave, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    const int source_group = shared_group_id(slave);
    ASSERT_GT(source_group, 0);

    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        if (unshare(CLONE_NEWUSER | CLONE_NEWNS) != 0) {
            _exit(1);
        }
        const char* points[] = {slave};
        PropagationTags tags = {};
        if (!read_propagation_snapshot(points, 1, &tags) || tags.shared >= 0 ||
            tags.master != source_group) {
            _exit(2);
        }
        _exit(0);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));

    best_effort_umount(slave);
    best_effort_umount(base);
    best_effort_rmdir(slave);
    best_effort_rmdir(base);
}

TEST_F(MountPropagationTest, OrdinaryUmountRejectsMountedDescendant) {
    char base[160] = {};
    char child_dir[192] = {};
    snprintf(base, sizeof(base), "%s/base", root_);
    snprintf(child_dir, sizeof(child_dir), "%s/host", base);

    ASSERT_EQ(0, ensure_dir(base)) << strerror(errno);
    ASSERT_EQ(0, mount("", base, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(child_dir)) << strerror(errno);
    ASSERT_EQ(0, mount("", child_dir, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, create_marker(child_dir, "child_marker")) << strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, umount(base));
    EXPECT_EQ(EBUSY, errno);
    EXPECT_TRUE(marker_exists(child_dir, "child_marker"));

    ASSERT_EQ(0, umount(child_dir)) << strerror(errno);
    ASSERT_EQ(0, umount(base)) << strerror(errno);
    best_effort_rmdir(child_dir);
    best_effort_rmdir(base);
}

TEST_F(MountPropagationTest, UmountUsesExactEdgeAfterChildBecomesPrivate) {
    char base[160] = {};
    char peer[160] = {};
    char child[192] = {};
    char peer_child[192] = {};
    snprintf(base, sizeof(base), "%s/base", root_);
    snprintf(peer, sizeof(peer), "%s/slave", root_);
    snprintf(child, sizeof(child), "%s/child", base);
    snprintf(peer_child, sizeof(peer_child), "%s/child", peer);

    ASSERT_EQ(0, ensure_dir(base)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(peer)) << strerror(errno);
    ASSERT_EQ(0, mount("", base, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, base, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(base, peer, nullptr, MS_BIND, nullptr)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(child)) << strerror(errno);
    ASSERT_EQ(0, mount("", child, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, create_marker(child, "propagated")) << strerror(errno);
    ASSERT_TRUE(marker_exists(peer_child, "propagated"));

    ASSERT_EQ(0, mount(nullptr, child, nullptr, MS_PRIVATE, nullptr)) << strerror(errno);
    ASSERT_EQ(0, umount2(child, MNT_DETACH)) << strerror(errno);
    ASSERT_EQ(0, create_marker(peer_child, "after_umount")) << strerror(errno);
    EXPECT_TRUE(marker_exists(child, "after_umount"));

    best_effort_umount(peer_child);
    best_effort_umount(peer);
    best_effort_umount(base);
    best_effort_rmdir(child);
    best_effort_rmdir(peer);
    best_effort_rmdir(base);
}

TEST_F(MountPropagationTest, LazyUmountPropagatesCompleteSourceSubtree) {
    char base[160] = {};
    char peer[160] = {};
    char child[192] = {};
    char peer_child[192] = {};
    char grandchild[224] = {};
    snprintf(base, sizeof(base), "%s/base", root_);
    snprintf(peer, sizeof(peer), "%s/slave", root_);
    snprintf(child, sizeof(child), "%s/child", base);
    snprintf(peer_child, sizeof(peer_child), "%s/child", peer);
    snprintf(grandchild, sizeof(grandchild), "%s/grandchild", child);

    ASSERT_EQ(0, ensure_dir(base)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(peer)) << strerror(errno);
    ASSERT_EQ(0, mount("", base, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, base, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(base, peer, nullptr, MS_BIND, nullptr)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(child)) << strerror(errno);
    ASSERT_EQ(0, mount("", child, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(grandchild)) << strerror(errno);
    ASSERT_EQ(0, mount("", grandchild, "ramfs", 0, nullptr)) << strerror(errno);

    ASSERT_EQ(0, umount2(child, MNT_DETACH)) << strerror(errno);
    ASSERT_EQ(0, create_marker(peer_child, "subtree_removed")) << strerror(errno);
    EXPECT_TRUE(marker_exists(child, "subtree_removed"));

    best_effort_umount(peer_child);
    best_effort_umount(peer);
    best_effort_umount(base);
    best_effort_rmdir(grandchild);
    best_effort_rmdir(child);
    best_effort_rmdir(peer);
    best_effort_rmdir(base);
}

TEST_F(MountPropagationTest, LazyUmountRemovesSelectedRootTopper) {
    char base[160] = {};
    char peer[160] = {};
    char child[192] = {};
    char peer_child[192] = {};
    snprintf(base, sizeof(base), "%s/base", root_);
    snprintf(peer, sizeof(peer), "%s/slave", root_);
    snprintf(child, sizeof(child), "%s/child", base);
    snprintf(peer_child, sizeof(peer_child), "%s/child", peer);

    ASSERT_EQ(0, ensure_dir(base)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(peer)) << strerror(errno);
    ASSERT_EQ(0, mount("", base, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, base, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(base, peer, nullptr, MS_BIND, nullptr)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(child)) << strerror(errno);
    ASSERT_EQ(0, create_marker(child, "underlay")) << strerror(errno);
    ASSERT_EQ(0, mount("", child, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, create_marker(child, "lower")) << strerror(errno);
    ASSERT_EQ(0, mount("", child, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, create_marker(child, "topper")) << strerror(errno);
    ASSERT_TRUE(marker_exists(peer_child, "topper"));

    ASSERT_EQ(0, umount2(base, MNT_DETACH)) << strerror(errno);
    EXPECT_TRUE(marker_exists(peer_child, "underlay"));
    EXPECT_FALSE(marker_exists(peer_child, "lower"));
    EXPECT_FALSE(marker_exists(peer_child, "topper"));

    best_effort_umount(peer_child);
    best_effort_umount(peer);
    best_effort_umount(base);
    best_effort_rmdir(child);
    best_effort_rmdir(peer);
    best_effort_rmdir(base);
}

TEST_F(MountPropagationTest, UmountRetainsPeerWithPrivateNonRootChild) {
    char base[160] = {};
    char peer[160] = {};
    char child[192] = {};
    char peer_child[192] = {};
    char peer_grandchild[224] = {};
    snprintf(base, sizeof(base), "%s/base", root_);
    snprintf(peer, sizeof(peer), "%s/slave", root_);
    snprintf(child, sizeof(child), "%s/child", base);
    snprintf(peer_child, sizeof(peer_child), "%s/child", peer);
    snprintf(peer_grandchild, sizeof(peer_grandchild), "%s/grandchild", peer_child);

    ASSERT_EQ(0, ensure_dir(base)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(peer)) << strerror(errno);
    ASSERT_EQ(0, mount("", base, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, base, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(base, peer, nullptr, MS_BIND, nullptr)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(child)) << strerror(errno);
    ASSERT_EQ(0, mount("", child, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, peer_child, nullptr, MS_PRIVATE, nullptr)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(peer_grandchild)) << strerror(errno);
    ASSERT_EQ(0, mount("", peer_grandchild, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, create_marker(peer_grandchild, "retained")) << strerror(errno);

    ASSERT_EQ(0, umount(child)) << strerror(errno);
    EXPECT_TRUE(marker_exists(peer_grandchild, "retained"));
    ASSERT_EQ(0, create_marker(peer_child, "peer_only")) << strerror(errno);
    EXPECT_FALSE(marker_exists(child, "peer_only"));

    best_effort_umount(peer_grandchild);
    best_effort_umount(peer_child);
    best_effort_umount(peer);
    best_effort_umount(base);
    best_effort_rmdir(peer_grandchild);
    best_effort_rmdir(child);
    best_effort_rmdir(peer);
    best_effort_rmdir(base);
}

TEST_F(MountPropagationTest, UmountRestoresPeerRootCover) {
    char base[160] = {};
    char peer[160] = {};
    char child[192] = {};
    char peer_child[192] = {};
    snprintf(base, sizeof(base), "%s/base", root_);
    snprintf(peer, sizeof(peer), "%s/slave", root_);
    snprintf(child, sizeof(child), "%s/child", base);
    snprintf(peer_child, sizeof(peer_child), "%s/child", peer);

    ASSERT_EQ(0, ensure_dir(base)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(peer)) << strerror(errno);
    ASSERT_EQ(0, mount("", base, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, base, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(base, peer, nullptr, MS_BIND, nullptr)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(child)) << strerror(errno);
    ASSERT_EQ(0, mount("", child, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, peer_child, nullptr, MS_PRIVATE, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount("", peer_child, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, create_marker(peer_child, "cover")) << strerror(errno);

    ASSERT_EQ(0, mount(nullptr, child, nullptr, MS_PRIVATE, nullptr)) << strerror(errno);
    ASSERT_EQ(0, umount2(child, MNT_DETACH)) << strerror(errno);
    EXPECT_TRUE(marker_exists(peer_child, "cover"));
    ASSERT_EQ(0, create_marker(peer_child, "cover_only")) << strerror(errno);
    EXPECT_FALSE(marker_exists(child, "cover_only"));

    best_effort_umount(peer_child);
    best_effort_umount(peer);
    best_effort_umount(base);
    best_effort_rmdir(child);
    best_effort_rmdir(peer);
    best_effort_rmdir(base);
}

TEST_F(MountPropagationTest, ParentUmountRemovesLockedCrossUserNamespaceCopy) {
    char base[160] = {};
    char host[192] = {};
    snprintf(base, sizeof(base), "%s/base", root_);
    snprintf(host, sizeof(host), "%s/host", base);

    ASSERT_EQ(0, ensure_dir(base)) << strerror(errno);
    ASSERT_EQ(0, mount(base, base, nullptr, MS_BIND, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, base, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(host)) << strerror(errno);
    ASSERT_EQ(0, mount("", host, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, create_marker(host, "propagated_marker")) << strerror(errno);

    int ready_pipe[2] = {-1, -1};
    int continue_pipe[2] = {-1, -1};
    ASSERT_EQ(0, pipe(ready_pipe)) << strerror(errno);
    ASSERT_EQ(0, pipe(continue_pipe)) << strerror(errno);

    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        close(ready_pipe[0]);
        close(continue_pipe[1]);
        if (unshare(CLONE_NEWUSER | CLONE_NEWNS) != 0 ||
            !marker_exists(host, "propagated_marker")) {
            _exit(1);
        }
        errno = 0;
        if (umount2(host, MNT_DETACH) != -1 || errno != EINVAL) {
            _exit(2);
        }
        if (write(ready_pipe[1], "r", 1) != 1) {
            _exit(3);
        }
        char token = 0;
        if (read(continue_pipe[0], &token, 1) != 1) {
            _exit(4);
        }
        _exit(marker_exists(host, "propagated_marker") ? 5 : 0);
    }

    close(ready_pipe[1]);
    close(continue_pipe[0]);
    char token = 0;
    ASSERT_EQ(1, read(ready_pipe[0], &token, 1)) << strerror(errno);
    errno = 0;
    const int umount_result = umount(host);
    const int umount_errno = errno;
    const int wake_result = write(continue_pipe[1], "c", 1);
    const int wake_errno = errno;
    close(ready_pipe[0]);
    close(continue_pipe[1]);

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, umount_result) << strerror(umount_errno);
    EXPECT_EQ(1, wake_result) << strerror(wake_errno);
    EXPECT_EQ(0, WEXITSTATUS(status));

    best_effort_umount(base);
    best_effort_rmdir(host);
    best_effort_rmdir(base);
}

TEST_F(MountPropagationTest, LazyDetachPreservesLockedPropagatedSubtree) {
    char base[160] = {};
    char child_dir[192] = {};
    char sibling_dir[192] = {};
    char grandchild_dir[224] = {};
    snprintf(base, sizeof(base), "%s/base", root_);
    snprintf(child_dir, sizeof(child_dir), "%s/child", base);
    snprintf(sibling_dir, sizeof(sibling_dir), "%s/sibling", base);
    snprintf(grandchild_dir, sizeof(grandchild_dir), "%s/grandchild", child_dir);

    ASSERT_EQ(0, ensure_dir(base)) << strerror(errno);
    ASSERT_EQ(0, mount(base, base, nullptr, MS_BIND, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, base, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(child_dir)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(sibling_dir)) << strerror(errno);
    ASSERT_EQ(0, mount("", child_dir, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, child_dir, nullptr, MS_PRIVATE, nullptr)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(grandchild_dir)) << strerror(errno);
    ASSERT_EQ(0, create_marker(grandchild_dir, "foo")) << strerror(errno);
    ASSERT_EQ(0, mount("", grandchild_dir, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, grandchild_dir, nullptr, MS_PRIVATE, nullptr)) << strerror(errno);

    int ready_pipe[2] = {-1, -1};
    int continue_pipe[2] = {-1, -1};
    ASSERT_EQ(0, pipe(ready_pipe)) << strerror(errno);
    ASSERT_EQ(0, pipe(continue_pipe)) << strerror(errno);

    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        close(ready_pipe[0]);
        close(continue_pipe[1]);
        if (unshare(CLONE_NEWUSER | CLONE_NEWNS) != 0 ||
            write(ready_pipe[1], "r", 1) != 1) {
            _exit(1);
        }
        char token = 0;
        if (read(continue_pipe[0], &token, 1) != 1) {
            _exit(2);
        }
        errno = 0;
        if (umount2(grandchild_dir, MNT_DETACH) != -1 || errno != EINVAL) {
            _exit(3);
        }
        const int dirfd = open(sibling_dir, O_RDONLY | O_DIRECTORY);
        if (dirfd < 0) {
            _exit(4);
        }
        if (umount2(sibling_dir, MNT_DETACH) != 0) {
            _exit(5);
        }
        errno = 0;
        const int fd = openat(dirfd, "grandchild/foo", O_RDONLY);
        const int open_errno = errno;
        if (fd >= 0) {
            close(fd);
        }
        close(dirfd);
        _exit(fd == -1 && open_errno == ENOENT ? 0 : 6);
    }

    close(ready_pipe[1]);
    close(continue_pipe[0]);
    char token = 0;
    ASSERT_EQ(1, read(ready_pipe[0], &token, 1)) << strerror(errno);
    errno = 0;
    const int bind_result =
        mount(child_dir, sibling_dir, nullptr, MS_BIND | MS_REC, nullptr);
    const int bind_errno = errno;
    errno = 0;
    const int private_result = bind_result == 0
                                   ? mount(nullptr, sibling_dir, nullptr,
                                           MS_PRIVATE | MS_REC, nullptr)
                                   : -1;
    const int private_errno = errno;
    const int wake_result = write(continue_pipe[1], "c", 1);
    const int wake_errno = errno;
    close(ready_pipe[0]);
    close(continue_pipe[1]);

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, bind_result) << strerror(bind_errno);
    EXPECT_EQ(0, private_result) << strerror(private_errno);
    EXPECT_EQ(1, wake_result) << strerror(wake_errno);
    EXPECT_EQ(0, WEXITSTATUS(status));

    best_effort_umount(sibling_dir);
    best_effort_umount(grandchild_dir);
    best_effort_umount(child_dir);
    best_effort_umount(base);
    best_effort_rmdir(grandchild_dir);
    best_effort_rmdir(child_dir);
    best_effort_rmdir(sibling_dir);
    best_effort_rmdir(base);
}

TEST_F(MountPropagationTest, SlaveReceivesMasterPropagationOnly) {
    char base[160] = {};
    char slave[160] = {};
    char host_sub[192] = {};
    char slave_host_sub[192] = {};
    char slave_local[192] = {};
    char base_local[192] = {};

    snprintf(base, sizeof(base), "%s/base", root_);
    snprintf(slave, sizeof(slave), "%s/slave", root_);
    snprintf(host_sub, sizeof(host_sub), "%s/host", base);
    snprintf(slave_host_sub, sizeof(slave_host_sub), "%s/host", slave);
    snprintf(slave_local, sizeof(slave_local), "%s/local", slave);
    snprintf(base_local, sizeof(base_local), "%s/local", base);

    ASSERT_EQ(0, ensure_dir(base)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(slave)) << strerror(errno);
    ASSERT_EQ(0, mount("", base, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, base, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(base, slave, nullptr, MS_BIND, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, slave, nullptr, MS_SLAVE, nullptr)) << strerror(errno);

    ASSERT_EQ(0, ensure_dir(host_sub)) << strerror(errno);
    ASSERT_EQ(0, mount("", host_sub, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, create_marker(host_sub, "host_marker")) << strerror(errno);
    EXPECT_TRUE(marker_exists(slave_host_sub, "host_marker"));

    ASSERT_EQ(0, ensure_dir(slave_local)) << strerror(errno);
    ASSERT_EQ(0, mount("", slave_local, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, create_marker(slave_local, "slave_marker")) << strerror(errno);
    EXPECT_FALSE(marker_exists(base_local, "slave_marker"));

    best_effort_umount(slave_local);
    best_effort_rmdir(slave_local);
    best_effort_umount(host_sub);
    best_effort_rmdir(host_sub);
    best_effort_umount(slave);
    best_effort_rmdir(slave);
    best_effort_umount(base);
    best_effort_rmdir(base);
}

TEST_F(MountPropagationTest, SharedSlaveKeepsPeerPropagation) {
    char base[160] = {};
    char slave_a[160] = {};
    char slave_b[160] = {};
    char host_sub[192] = {};
    char slave_a_host[192] = {};
    char slave_b_host[192] = {};
    char slave_a_local[192] = {};
    char slave_b_local[192] = {};
    char base_local[192] = {};

    snprintf(base, sizeof(base), "%s/base", root_);
    snprintf(slave_a, sizeof(slave_a), "%s/slave_a", root_);
    snprintf(slave_b, sizeof(slave_b), "%s/slave_b", root_);
    snprintf(host_sub, sizeof(host_sub), "%s/host", base);
    snprintf(slave_a_host, sizeof(slave_a_host), "%s/host", slave_a);
    snprintf(slave_b_host, sizeof(slave_b_host), "%s/host", slave_b);
    snprintf(slave_a_local, sizeof(slave_a_local), "%s/local", slave_a);
    snprintf(slave_b_local, sizeof(slave_b_local), "%s/local", slave_b);
    snprintf(base_local, sizeof(base_local), "%s/local", base);

    ASSERT_EQ(0, ensure_dir(base)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(slave_a)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(slave_b)) << strerror(errno);
    ASSERT_EQ(0, mount("", base, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, base, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(base, slave_a, nullptr, MS_BIND, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, slave_a, nullptr, MS_SLAVE, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, slave_a, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(slave_a, slave_b, nullptr, MS_BIND, nullptr)) << strerror(errno);

    ASSERT_EQ(0, ensure_dir(host_sub)) << strerror(errno);
    ASSERT_EQ(0, mount("", host_sub, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, create_marker(host_sub, "host_marker")) << strerror(errno);
    EXPECT_TRUE(marker_exists(slave_a_host, "host_marker"));
    EXPECT_TRUE(marker_exists(slave_b_host, "host_marker"));

    ASSERT_EQ(0, ensure_dir(slave_a_local)) << strerror(errno);
    ASSERT_EQ(0, mount("", slave_a_local, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, create_marker(slave_a_local, "slave_marker")) << strerror(errno);
    EXPECT_TRUE(marker_exists(slave_b_local, "slave_marker"));
    EXPECT_FALSE(marker_exists(base_local, "slave_marker"));

    best_effort_umount(slave_a_local);
    best_effort_rmdir(slave_a_local);
    best_effort_umount(host_sub);
    best_effort_rmdir(host_sub);
    best_effort_umount(slave_b);
    best_effort_rmdir(slave_b);
    best_effort_umount(slave_a);
    best_effort_rmdir(slave_a);
    best_effort_umount(base);
    best_effort_rmdir(base);
}

TEST_F(MountPropagationTest, DeepSlaveUnmountFollowsImmediateMasterChain) {
    char master[160] = {};
    char middle[160] = {};
    char leaf[160] = {};
    char master_host[192] = {};
    char middle_host[192] = {};
    char leaf_host[192] = {};
    snprintf(master, sizeof(master), "%s/base", root_);
    snprintf(middle, sizeof(middle), "%s/slave_a", root_);
    snprintf(leaf, sizeof(leaf), "%s/slave_b", root_);
    snprintf(master_host, sizeof(master_host), "%s/host", master);
    snprintf(middle_host, sizeof(middle_host), "%s/host", middle);
    snprintf(leaf_host, sizeof(leaf_host), "%s/host", leaf);

    ASSERT_EQ(0, ensure_dir(master)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(middle)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(leaf)) << strerror(errno);
    ASSERT_EQ(0, mount("", master, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, master, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(master, middle, nullptr, MS_BIND, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, middle, nullptr, MS_SLAVE, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, middle, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(middle, leaf, nullptr, MS_BIND, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, leaf, nullptr, MS_SLAVE, nullptr)) << strerror(errno);

    ASSERT_EQ(0, ensure_dir(master_host)) << strerror(errno);
    ASSERT_EQ(0, mount("", master_host, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, create_marker(master_host, "chain_marker")) << strerror(errno);
    ASSERT_TRUE(marker_exists(middle_host, "chain_marker"));
    ASSERT_TRUE(marker_exists(leaf_host, "chain_marker"));

    ASSERT_EQ(0, umount(master_host)) << strerror(errno);
    EXPECT_FALSE(marker_exists(master_host, "chain_marker"));
    EXPECT_FALSE(marker_exists(middle_host, "chain_marker"));
    EXPECT_FALSE(marker_exists(leaf_host, "chain_marker"));

    best_effort_umount(leaf);
    best_effort_umount(middle);
    best_effort_umount(master);
}

TEST_F(MountPropagationTest, BindSharedSourceIntoSharedTargetUpdatesPropagatedClone) {
    char src[160] = {};
    char target_a[160] = {};
    char target_b[160] = {};
    char bind_a[192] = {};
    char bind_b[192] = {};
    char src_host[192] = {};
    char bind_a_host[224] = {};
    char bind_b_host[224] = {};

    snprintf(src, sizeof(src), "%s/src", root_);
    snprintf(target_a, sizeof(target_a), "%s/target_a", root_);
    snprintf(target_b, sizeof(target_b), "%s/target_b", root_);
    snprintf(bind_a, sizeof(bind_a), "%s/bind", target_a);
    snprintf(bind_b, sizeof(bind_b), "%s/bind", target_b);
    snprintf(src_host, sizeof(src_host), "%s/host", src);
    snprintf(bind_a_host, sizeof(bind_a_host), "%s/host", bind_a);
    snprintf(bind_b_host, sizeof(bind_b_host), "%s/host", bind_b);

    ASSERT_EQ(0, ensure_dir(src)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(target_a)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(target_b)) << strerror(errno);
    ASSERT_EQ(0, mount("", src, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount("", target_a, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, src, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, target_a, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(target_a, target_b, nullptr, MS_BIND, nullptr)) << strerror(errno);

    ASSERT_EQ(0, ensure_dir(bind_a)) << strerror(errno);
    ASSERT_EQ(0, mount(src, bind_a, nullptr, MS_BIND, nullptr)) << strerror(errno);

    ASSERT_EQ(0, ensure_dir(src_host)) << strerror(errno);
    ASSERT_EQ(0, mount("", src_host, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, create_marker(src_host, "host_marker")) << strerror(errno);
    EXPECT_TRUE(marker_exists(bind_a_host, "host_marker"));
    EXPECT_TRUE(marker_exists(bind_b_host, "host_marker"));

    best_effort_umount(src_host);
    best_effort_rmdir(src_host);
    best_effort_umount(bind_a);
    best_effort_rmdir(bind_a);
    best_effort_umount(target_b);
    best_effort_rmdir(target_b);
    best_effort_umount(target_a);
    best_effort_rmdir(target_a);
    best_effort_umount(src);
    best_effort_rmdir(src);
}

TEST_F(MountPropagationTest, BindSharedSlaveIntoSharedTargetRegistersPropagatedCloneAsSlave) {
    char master[160] = {};
    char source[160] = {};
    char target_a[160] = {};
    char target_b[160] = {};
    char bind_a[192] = {};
    char bind_b[192] = {};
    char master_host[192] = {};
    char source_host[192] = {};
    char bind_a_host[224] = {};
    char bind_b_host[224] = {};

    snprintf(master, sizeof(master), "%s/master", root_);
    snprintf(source, sizeof(source), "%s/source", root_);
    snprintf(target_a, sizeof(target_a), "%s/target_a", root_);
    snprintf(target_b, sizeof(target_b), "%s/target_b", root_);
    snprintf(bind_a, sizeof(bind_a), "%s/bind", target_a);
    snprintf(bind_b, sizeof(bind_b), "%s/bind", target_b);
    snprintf(master_host, sizeof(master_host), "%s/host", master);
    snprintf(source_host, sizeof(source_host), "%s/host", source);
    snprintf(bind_a_host, sizeof(bind_a_host), "%s/host", bind_a);
    snprintf(bind_b_host, sizeof(bind_b_host), "%s/host", bind_b);

    ASSERT_EQ(0, ensure_dir(master)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(source)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(target_a)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(target_b)) << strerror(errno);
    ASSERT_EQ(0, mount("", master, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount("", target_a, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, master, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(master, source, nullptr, MS_BIND, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, source, nullptr, MS_SLAVE, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, source, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(nullptr, target_a, nullptr, MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount(target_a, target_b, nullptr, MS_BIND, nullptr)) << strerror(errno);

    ASSERT_EQ(0, ensure_dir(bind_a)) << strerror(errno);
    ASSERT_EQ(0, mount(source, bind_a, nullptr, MS_BIND, nullptr)) << strerror(errno);

    ASSERT_EQ(0, ensure_dir(master_host)) << strerror(errno);
    ASSERT_EQ(0, mount("", master_host, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, create_marker(master_host, "host_marker")) << strerror(errno);
    EXPECT_TRUE(marker_exists(source_host, "host_marker"));
    EXPECT_TRUE(marker_exists(bind_a_host, "host_marker"));
    EXPECT_TRUE(marker_exists(bind_b_host, "host_marker"));

    best_effort_umount(master_host);
    best_effort_rmdir(master_host);
    best_effort_umount(bind_a);
    best_effort_rmdir(bind_a);
    best_effort_umount(target_b);
    best_effort_rmdir(target_b);
    best_effort_umount(target_a);
    best_effort_rmdir(target_a);
    best_effort_umount(source);
    best_effort_rmdir(source);
    best_effort_umount(master);
    best_effort_rmdir(master);
}

TEST_F(MountPropagationTest, RecursiveTypeChangesMatchLinuxMountinfoSemantics) {
    char base[160] = {};
    char child[192] = {};
    char grandchild[224] = {};
    snprintf(base, sizeof(base), "%s/base", root_);
    snprintf(child, sizeof(child), "%s/child", base);
    snprintf(grandchild, sizeof(grandchild), "%s/grandchild", child);

    ASSERT_EQ(0, ensure_dir(base)) << strerror(errno);
    ASSERT_EQ(0, mount("", base, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(child)) << strerror(errno);
    ASSERT_EQ(0, mount("", child, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(grandchild)) << strerror(errno);
    ASSERT_EQ(0, mount("", grandchild, "ramfs", 0, nullptr)) << strerror(errno);

    const char* paths[] = {base, child, grandchild};
    PropagationTags tags[3] = {};

    ASSERT_EQ(0, mount(nullptr, base, nullptr, MS_REC | MS_SHARED, nullptr)) << strerror(errno);
    ASSERT_TRUE(read_propagation_snapshot(paths, 3, tags));
    EXPECT_GT(tags[0].shared, 0);
    EXPECT_GT(tags[1].shared, 0);
    EXPECT_GT(tags[2].shared, 0);
    EXPECT_NE(tags[0].shared, tags[1].shared);
    EXPECT_NE(tags[0].shared, tags[2].shared);
    EXPECT_NE(tags[1].shared, tags[2].shared);

    // Linux do_make_slave() turns a singleton shared group with no existing
    // master into private; MS_SLAVE does not manufacture a master relation.
    ASSERT_EQ(0, mount(nullptr, base, nullptr, MS_REC | MS_SLAVE, nullptr)) << strerror(errno);
    ASSERT_TRUE(read_propagation_snapshot(paths, 3, tags));
    for (const auto& tag : tags) {
        EXPECT_EQ(-1, tag.shared);
        EXPECT_EQ(-1, tag.master);
        EXPECT_FALSE(tag.unbindable);
    }

    ASSERT_EQ(0, mount(nullptr, base, nullptr, MS_REC | MS_UNBINDABLE, nullptr))
        << strerror(errno);
    ASSERT_TRUE(read_propagation_snapshot(paths, 3, tags));
    for (const auto& tag : tags) {
        EXPECT_EQ(-1, tag.shared);
        EXPECT_EQ(-1, tag.master);
        EXPECT_TRUE(tag.unbindable);
    }

    ASSERT_EQ(0, mount(nullptr, base, nullptr, MS_REC | MS_PRIVATE, nullptr)) << strerror(errno);
    ASSERT_TRUE(read_propagation_snapshot(paths, 3, tags));
    for (const auto& tag : tags) {
        EXPECT_EQ(-1, tag.shared);
        EXPECT_EQ(-1, tag.master);
        EXPECT_EQ(-1, tag.propagate_from);
        EXPECT_FALSE(tag.unbindable);
    }
}

TEST_F(MountPropagationTest, RecursiveChangesAreAtomicAgainstSnapshotsAndNamespaceCopy) {
    char base[160] = {};
    char child[192] = {};
    char grandchild[224] = {};
    char dynamic[192] = {};
    snprintf(base, sizeof(base), "%s/base", root_);
    snprintf(child, sizeof(child), "%s/child", base);
    snprintf(grandchild, sizeof(grandchild), "%s/grandchild", child);
    snprintf(dynamic, sizeof(dynamic), "%s/dynamic", base);

    ASSERT_EQ(0, ensure_dir(base)) << strerror(errno);
    ASSERT_EQ(0, mount("", base, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(child)) << strerror(errno);
    ASSERT_EQ(0, mount("", child, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(grandchild)) << strerror(errno);
    ASSERT_EQ(0, mount("", grandchild, "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(dynamic)) << strerror(errno);

    int start_pipe[2] = {-1, -1};
    ASSERT_EQ(0, pipe(start_pipe)) << strerror(errno);
    int activity_pipe[2] = {-1, -1};
    if (pipe(activity_pipe) != 0) {
        const int error = errno;
        close(start_pipe[0]);
        close(start_pipe[1]);
        FAIL() << "activity pipe: " << strerror(error);
        return;
    }
    int ready_pipe[2] = {-1, -1};
    int worker_activity_pipe[2] = {-1, -1};
    int worker_done_pipe[2] = {-1, -1};
    if (pipe(ready_pipe) != 0 || pipe(worker_activity_pipe) != 0 ||
        pipe(worker_done_pipe) != 0) {
        const int error = errno;
        close(start_pipe[0]);
        close(start_pipe[1]);
        close(activity_pipe[0]);
        close(activity_pipe[1]);
        close(ready_pipe[0]);
        close(ready_pipe[1]);
        close(worker_activity_pipe[0]);
        close(worker_activity_pipe[1]);
        close(worker_done_pipe[0]);
        close(worker_done_pipe[1]);
        FAIL() << "worker coordination pipe: " << strerror(error);
        return;
    }
    pid_t children[4] = {};
    auto abort_workers = [&]() {
        close(start_pipe[0]);
        close(start_pipe[1]);
        close(activity_pipe[0]);
        close(activity_pipe[1]);
        close(ready_pipe[0]);
        close(ready_pipe[1]);
        close(worker_activity_pipe[0]);
        close(worker_activity_pipe[1]);
        close(worker_done_pipe[0]);
        close(worker_done_pipe[1]);
        terminate_children(children, 4);
    };

    children[0] = fork();
    if (children[0] < 0) {
        const int error = errno;
        abort_workers();
        FAIL() << "fork toggler: " << strerror(error);
        return;
    }
    if (children[0] == 0) {
        close(start_pipe[1]);
        close(activity_pipe[0]);
        close(ready_pipe[1]);
        close(worker_activity_pipe[0]);
        close(worker_done_pipe[1]);
        char token = 0;
        if (!read_exact(start_pipe[0], &token, 1)) {
            _exit(1);
        }
        char ready[3] = {};
        if (!read_exact(ready_pipe[0], ready, sizeof(ready)) ||
            !write_exact(activity_pipe[1], "A", 1)) {
            _exit(2);
        }
        if (!read_exact(ready_pipe[0], &token, 1) ||
            !write_exact(worker_activity_pipe[1], "AA", 2)) {
            _exit(3);
        }
        for (int i = 0; i < 1024; ++i) {
            const unsigned long type = (i & 1) == 0 ? MS_SHARED : MS_PRIVATE;
            if (mount(nullptr, base, nullptr, MS_REC | type, nullptr) != 0) {
                _exit(3);
            }
            if ((i & 15) == 15) {
                sched_yield();
            }
        }
        char done[2] = {};
        if (!read_exact(worker_done_pipe[0], done, sizeof(done)) ||
            !write_exact(activity_pipe[1], "D", 1)) {
            _exit(4);
        }
        _exit(0);
    }

    children[1] = fork();
    if (children[1] < 0) {
        const int error = errno;
        abort_workers();
        FAIL() << "fork observer: " << strerror(error);
        return;
    }
    if (children[1] == 0) {
        close(start_pipe[1]);
        close(activity_pipe[1]);
        close(ready_pipe[0]);
        close(worker_activity_pipe[0]);
        close(worker_activity_pipe[1]);
        close(worker_done_pipe[0]);
        close(worker_done_pipe[1]);
        char token = 0;
        if (!read_exact(start_pipe[0], &token, 1) || !write_exact(ready_pipe[1], "R", 1)) {
            _exit(5);
        }
        if (!read_exact(activity_pipe[0], &token, 1) || token != 'A') {
            _exit(6);
        }
        const int flags = fcntl(activity_pipe[0], F_GETFL, 0);
        if (flags < 0 || fcntl(activity_pipe[0], F_SETFL, flags | O_NONBLOCK) != 0) {
            _exit(7);
        }
        if (!write_exact(ready_pipe[1], "O", 1)) {
            _exit(8);
        }
        const char* paths[] = {base, child, grandchild};
        int samples = 0;
        bool saw_shared = false;
        bool saw_private = false;
        for (;;) {
            const ssize_t status = read(activity_pipe[0], &token, 1);
            if (status == 1 && token == 'D') {
                break;
            }
            if (status == 0 || (status < 0 && errno != EAGAIN && errno != EINTR)) {
                _exit(9);
            }
            PropagationTags tags[3] = {};
            if (!read_propagation_snapshot(paths, 3, tags) || !snapshot_is_uniform(tags, 3)) {
                _exit(10);
            }
            ++samples;
            saw_shared = saw_shared || tags[0].shared > 0;
            saw_private = saw_private || tags[0].shared < 0;
        }
        if (samples == 0 || !saw_shared || !saw_private) {
            _exit(11);
        }
        _exit(0);
    }

    children[2] = fork();
    if (children[2] < 0) {
        const int error = errno;
        abort_workers();
        FAIL() << "fork namespace copier: " << strerror(error);
        return;
    }
    if (children[2] == 0) {
        close(start_pipe[1]);
        close(activity_pipe[0]);
        close(activity_pipe[1]);
        close(ready_pipe[0]);
        close(worker_activity_pipe[1]);
        close(worker_done_pipe[0]);
        char token = 0;
        if (!read_exact(start_pipe[0], &token, 1) || !write_exact(ready_pipe[1], "R", 1) ||
            !read_exact(worker_activity_pipe[0], &token, 1)) {
            _exit(11);
        }
        if (unshare(CLONE_NEWNS) != 0) {
            _exit(12);
        }
        const char* paths[] = {base, child, grandchild};
        PropagationTags tags[3] = {};
        if (!read_propagation_snapshot(paths, 3, tags) || !snapshot_is_uniform(tags, 3)) {
            _exit(13);
        }
        if (!write_exact(worker_done_pipe[1], "D", 1)) {
            _exit(14);
        }
        _exit(0);
    }

    children[3] = fork();
    if (children[3] < 0) {
        const int error = errno;
        abort_workers();
        FAIL() << "fork topology worker: " << strerror(error);
        return;
    }
    if (children[3] == 0) {
        close(start_pipe[1]);
        close(activity_pipe[0]);
        close(activity_pipe[1]);
        close(ready_pipe[0]);
        close(worker_activity_pipe[1]);
        close(worker_done_pipe[0]);
        char token = 0;
        if (!read_exact(start_pipe[0], &token, 1) || !write_exact(ready_pipe[1], "R", 1) ||
            !read_exact(worker_activity_pipe[0], &token, 1)) {
            _exit(15);
        }
        for (int i = 0; i < 32; ++i) {
            if (mount("", dynamic, "ramfs", 0, nullptr) != 0 || umount(dynamic) != 0) {
                _exit(16);
            }
        }
        if (!write_exact(worker_done_pipe[1], "D", 1)) {
            _exit(17);
        }
        _exit(0);
    }

    close(start_pipe[0]);
    close(activity_pipe[0]);
    close(activity_pipe[1]);
    close(ready_pipe[0]);
    close(ready_pipe[1]);
    close(worker_activity_pipe[0]);
    close(worker_activity_pipe[1]);
    close(worker_done_pipe[0]);
    close(worker_done_pipe[1]);
    if (!write_exact(start_pipe[1], "ssss", 4)) {
        const int error = errno;
        close(start_pipe[1]);
        terminate_children(children, 4);
        FAIL() << "release workers: " << strerror(error);
        return;
    }
    close(start_pipe[1]);
    EXPECT_TRUE(wait_children_until(children, 4, 20));

    // Establish one canonical final state after the topology mutator stops;
    // mounts raced into the tree are allowed to inherit an earlier state.
    ASSERT_EQ(0, mount(nullptr, base, nullptr, MS_REC | MS_PRIVATE, nullptr)) << strerror(errno);
    const char* paths[] = {base, child, grandchild};
    PropagationTags tags[3] = {};
    ASSERT_TRUE(read_propagation_snapshot(paths, 3, tags));
    EXPECT_TRUE(snapshot_is_uniform(tags, 3));
    EXPECT_EQ(-1, tags[0].shared);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
