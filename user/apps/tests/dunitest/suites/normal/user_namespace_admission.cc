#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#include <string>
#include <vector>

#include "user_namespace_test_support.h"

namespace {

using Scenario = std::string (*)(const char*);

std::string failure(const char* stage) {
    return std::string(stage) + ": errno=" + std::to_string(errno) + " (" + strerror(errno) + ")";
}

bool wait_bounded(pid_t child, int* status, unsigned polls = 200) {
    for (unsigned i = 0; i < polls; ++i) {
        const pid_t result = waitpid(child, status, WNOHANG);
        if (result == child) return true;
        if (result < 0 && errno != EINTR) return false;
        usleep(100000);
    }
    return false;
}

int clone_child(void*) { return 0; }

// clone() needs its own stack even though the new child exits immediately.
int create_clone(int* child_status) {
    std::vector<char> stack(128 * 1024);
    const pid_t child = clone(clone_child, stack.data() + stack.size(), CLONE_NEWUSER | SIGCHLD, nullptr);
    if (child < 0) return -1;
    if (!wait_bounded(child, child_status)) {
        kill(child, SIGKILL);
        wait_bounded(child, child_status, 20);
        errno = ETIMEDOUT;
        return -1;
    }
    if (!WIFEXITED(*child_status) || WEXITSTATUS(*child_status) != 0) {
        errno = ECHILD;
        return -1;
    }
    return 0;
}

std::string namespace_identity(int proc_self, struct stat* identity) {
    const int fd = openat(proc_self, "ns/user", O_RDONLY | O_CLOEXEC);
    if (fd < 0) return failure("open user namespace identity");
    const int result = fstat(fd, identity);
    const int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    return result == 0 ? std::string() : failure("stat user namespace identity");
}

std::string reject_creation(int expected, bool use_clone, int proc_self) {
    struct stat before {}, after {};
    auto error = namespace_identity(proc_self, &before);
    if (!error.empty()) return error;
    int child_status = 0;
    errno = 0;
    const int result = use_clone ? create_clone(&child_status) : unshare(CLONE_NEWUSER);
    const int actual_errno = errno;
    if (result != -1 || actual_errno != expected) {
        return std::string(use_clone ? "clone" : "unshare") + " admission expected errno="
            + std::to_string(expected) + ", result=" + std::to_string(result)
            + ", errno=" + std::to_string(actual_errno);
    }
    error = namespace_identity(proc_self, &after);
    if (!error.empty()) return error;
    if (before.st_dev != after.st_dev || before.st_ino != after.st_ino)
        return "rejected creation changed user namespace";
    return {};
}

std::string root_unshare(const char*) {
    if (chroot("/") != 0) return failure("chroot normal root");
    if (unshare(CLONE_NEWUSER) != 0) return failure("normal-root unshare");
    return {};
}

std::string root_clone(const char*) {
    if (chroot("/") != 0) return failure("chroot normal root");
    int status = 0;
    if (create_clone(&status) != 0) return failure("normal-root clone");
    return {};
}

std::string chroot_creation(const char* directory, bool use_clone) {
    const int proc_self = open("/proc/self", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (proc_self < 0) return failure("open proc before chroot");
    if (chroot(directory) != 0) {
        const auto error = failure("chroot strict subdirectory");
        close(proc_self);
        return error;
    }
    // Deliberately retain cwd outside the new root: admission depends on root,
    // not on cwd or the existence of open descriptors to the outside.
    const auto error = reject_creation(EPERM, use_clone, proc_self);
    close(proc_self);
    return error;
}

std::string chroot_unshare(const char* directory) { return chroot_creation(directory, false); }
std::string chroot_clone(const char* directory) { return chroot_creation(directory, true); }

std::string mount_namespace_root(const char*) {
    if (unshare(CLONE_NEWNS) != 0) return failure("copy mount namespace");
    if (unshare(CLONE_NEWUSER) != 0) return failure("new mount namespace normal root");
    return {};
}

std::string covered_root(const char*) {
    if (unshare(CLONE_NEWNS) != 0) return failure("isolate covered root");
    if (mount(nullptr, "/", nullptr, MS_REC | MS_PRIVATE, nullptr) != 0)
        return failure("make mount namespace private");
    const int proc_self = open("/proc/self", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (proc_self < 0) return failure("open proc before root cover");
    if (mount("/", "/", nullptr, MS_BIND, nullptr) != 0) {
        const auto error = failure("bind cover namespace root");
        close(proc_self);
        return error;
    }
    // The fs root still names the underlying mount. Linux current_chrooted()
    // follows the namespace root cover, so the two path identities differ.
    // Do not chroot("/"): Linux's pure-root lookup would keep this old root.
    auto error = reject_creation(EPERM, false, proc_self);
    if (error.empty()) error = reject_creation(EPERM, true, proc_self);
    close(proc_self);
    return error;
}

std::string deepest_namespace(const char*) {
    const int proc_self = open("/proc/self", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (proc_self < 0) return failure("open proc before nesting");
    std::string error;
    // This suite runs in the CI guest's initial user namespace. Each child
    // needs real self mappings before it can create another user namespace.
    for (unsigned level = 1; level <= 33; ++level) {
        const uid_t uid = geteuid();
        const gid_t gid = getegid();
        if (unshare(CLONE_NEWUSER) != 0) {
            error = failure(("unshare level " + std::to_string(level)).c_str());
            break;
        }
        if (!dunitest::install_self_user_namespace_maps(uid, gid)) {
            error = failure(("install mappings level " + std::to_string(level)).c_str());
            break;
        }
        if (geteuid() != 0 || getegid() != 0) {
            error = "self mappings failed at level " + std::to_string(level);
            break;
        }
        if (level == 32) {
            int status = 0;
            if (create_clone(&status) != 0) {
                error = failure("clone allowed child level 33");
                break;
            }
        }
    }
    if (error.empty()) error = reject_creation(ENOSPC, true, proc_self);
    if (error.empty()) error = reject_creation(ENOSPC, false, proc_self);
    close(proc_self);
    return error;
}

void run_isolated(Scenario scenario) {
    char directory[] = "/tmp/userns-admission-XXXXXX";
    ASSERT_NE(nullptr, mkdtemp(directory));
    int channel[2];
    if (pipe2(channel, O_CLOEXEC | O_NONBLOCK) != 0) {
        const auto error = failure("report pipe");
        rmdir(directory);
        FAIL() << error;
    }
    const pid_t child = fork();
    if (child < 0) {
        const auto error = failure("scenario fork");
        close(channel[0]);
        close(channel[1]);
        rmdir(directory);
        FAIL() << error;
    }
    if (child == 0) {
        close(channel[0]);
        const auto error = scenario(directory);
        if (!error.empty()) {
            const size_t size = error.size() < 1024 ? error.size() : 1024;
            const ssize_t ignored = write(channel[1], error.data(), size);
            (void)ignored;
        }
        close(channel[1]);
        _exit(error.empty() ? 0 : 1);
    }
    close(channel[1]);
    int status = 0;
    const bool exited = wait_bounded(child, &status);
    if (!exited) {
        kill(child, SIGKILL);
        wait_bounded(child, &status, 20);
    }
    char report[1025] = {};
    const ssize_t size = read(channel[0], report, sizeof(report) - 1);
    close(channel[0]);
    rmdir(directory);
    ASSERT_TRUE(exited) << "isolated namespace scenario timed out";
    ASSERT_TRUE(WIFEXITED(status)) << "scenario terminated by signal";
    EXPECT_EQ(0, WEXITSTATUS(status)) << (size > 0 ? report : "no scenario diagnostic");
}

TEST(UserNamespaceAdmission, NormalRootUnshareSucceeds) { run_isolated(root_unshare); }
TEST(UserNamespaceAdmission, NormalRootCloneSucceeds) { run_isolated(root_clone); }
TEST(UserNamespaceAdmission, ChrootUnshareIsRejected) { run_isolated(chroot_unshare); }
TEST(UserNamespaceAdmission, ChrootCloneIsRejected) { run_isolated(chroot_clone); }
TEST(UserNamespaceAdmission, CopiedMountNamespaceRootSucceeds) { run_isolated(mount_namespace_root); }
TEST(UserNamespaceAdmission, CoveredRootRejectsHiddenFsRoot) { run_isolated(covered_root); }
TEST(UserNamespaceAdmission, Level33AllowedAndLevel34Rejected) { run_isolated(deepest_namespace); }

} // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
