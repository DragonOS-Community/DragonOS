#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdbool.h>
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#include <string>

#ifndef SYS_pivot_root
#ifdef __NR_pivot_root
#define SYS_pivot_root __NR_pivot_root
#elif defined(__x86_64__)
#define SYS_pivot_root 155
#else
#define SYS_pivot_root 41
#endif
#endif

#ifndef CLONE_NEWNS
#define CLONE_NEWNS 0x00020000
#endif

#ifndef MS_REC
#define MS_REC 16384
#endif

namespace {

constexpr int kSkipExitCode = 77;
int g_child_status_fd = -1;

using PivotRootCaseFn = void (*)();

void write_child_status(const char* kind, const char* reason) {
    if (g_child_status_fd < 0) {
        return;
    }

    dprintf(g_child_status_fd, "%s:%s", kind, reason == nullptr ? "" : reason);
}

[[noreturn]] void child_pass() {
    _exit(0);
}

[[noreturn]] void child_skip(const char* reason) {
    write_child_status("SKIP", reason);
    _exit(kSkipExitCode);
}

[[noreturn]] void child_fail(const char* reason) {
    write_child_status("FAIL", reason);
    _exit(1);
}

int ensure_dir(const char* path) {
    struct stat st = {};
    if (stat(path, &st) == 0) {
        return S_ISDIR(st.st_mode) ? 0 : -1;
    }
    return mkdir(path, 0755);
}

void ensure_parent_tree() {
    ensure_dir("/tmp");
    ensure_dir("/tmp/test_pivot_root");
}

void cleanup_mount(const char* path) {
    umount(path);
    rmdir(path);
}

long do_pivot_root(const char* new_root, const char* put_old) {
    return syscall(SYS_pivot_root, new_root, put_old);
}

void prepare_private_mount_namespace() {
    if (unshare(CLONE_NEWNS) != 0) {
        child_skip(strerror(errno));
    }

    if (mount(nullptr, "/", nullptr, MS_REC | MS_PRIVATE, nullptr) != 0) {
        child_skip(strerror(errno));
    }
}

std::string read_all_from_fd(int fd) {
    std::string out;
    char buf[256];
    ssize_t n = 0;

    while ((n = read(fd, buf, sizeof(buf))) > 0) {
        out.append(buf, static_cast<size_t>(n));
    }

    return out;
}

void expect_case_pass_or_skip(const char* case_name, PivotRootCaseFn fn) {
    int pipefd[2] = {-1, -1};
    ASSERT_EQ(0, pipe(pipefd)) << case_name << ": pipe failed: errno=" << errno << " ("
                               << strerror(errno) << ")";

    pid_t child = fork();
    ASSERT_GE(child, 0) << case_name << ": fork failed: errno=" << errno << " ("
                        << strerror(errno) << ")";

    if (child == 0) {
        close(pipefd[0]);
        g_child_status_fd = pipefd[1];
        fn();
        child_fail("case returned without explicit status");
    }

    close(pipefd[1]);

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << case_name << ": waitpid failed: errno="
                                                 << errno << " (" << strerror(errno) << ")";

    std::string detail = read_all_from_fd(pipefd[0]);
    close(pipefd[0]);

    ASSERT_TRUE(WIFEXITED(status)) << case_name << ": child terminated abnormally";

    const int exit_code = WEXITSTATUS(status);
    if (exit_code == kSkipExitCode) {
        GTEST_SKIP() << case_name << ": " << detail;
    }

    EXPECT_EQ(0, exit_code) << case_name << ": " << detail;
}

void case_success_path() {
    const char* new_root = "/tmp/test_pivot_root/success/newroot";
    const char* put_old = "oldroot";
    const char* oldroot_abs = "/tmp/test_pivot_root/success/newroot/oldroot";
    const char* bin_dir = "/tmp/test_pivot_root/success/newroot/bin";
    char cwd[256] = {};

    ensure_parent_tree();
    ensure_dir("/tmp/test_pivot_root/success");
    ensure_dir(new_root);
    prepare_private_mount_namespace();

    if (mount("", new_root, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }

    if (mkdir(oldroot_abs, 0755) != 0 && errno != EEXIST) {
        cleanup_mount(new_root);
        child_fail("mkdir(oldroot) failed");
    }

    if (mkdir(bin_dir, 0755) != 0 && errno != EEXIST) {
        cleanup_mount(new_root);
        child_fail("mkdir(bin) failed");
    }

    if (chdir(new_root) != 0) {
        cleanup_mount(new_root);
        child_fail("chdir(new_root) failed");
    }

    if (do_pivot_root(".", put_old) != 0) {
        child_fail(strerror(errno));
    }

    if (getcwd(cwd, sizeof(cwd)) == nullptr) {
        child_fail("getcwd failed after pivot");
    }

    if (strcmp(cwd, "/") != 0) {
        child_fail("cwd is not / after pivot");
    }

    if (access("/oldroot", F_OK) != 0) {
        child_fail("old root not reachable under /oldroot");
    }

    if (access("/bin", F_OK) != 0) {
        child_fail("new root is not visible via absolute path");
    }

    child_pass();
}

void case_dot_dot_path() {
    const char* new_root = "/tmp/test_pivot_root/dotdot/newroot";
    const char* bin_dir = "/tmp/test_pivot_root/dotdot/newroot/bin";
    int oldroot_fd = -1;
    int newroot_fd = -1;
    char cwd[256] = {};

    ensure_parent_tree();
    ensure_dir("/tmp/test_pivot_root/dotdot");
    ensure_dir(new_root);
    prepare_private_mount_namespace();

    if (mount("", new_root, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }

    if (mkdir(bin_dir, 0755) != 0 && errno != EEXIST) {
        cleanup_mount(new_root);
        child_fail("mkdir(bin) failed");
    }

    oldroot_fd = open("/", O_DIRECTORY | O_RDONLY);
    if (oldroot_fd < 0) {
        cleanup_mount(new_root);
        child_fail("open(oldroot) failed");
    }

    newroot_fd = open(new_root, O_DIRECTORY | O_RDONLY);
    if (newroot_fd < 0) {
        close(oldroot_fd);
        cleanup_mount(new_root);
        child_fail("open(newroot) failed");
    }

    if (fchdir(newroot_fd) != 0) {
        close(newroot_fd);
        close(oldroot_fd);
        cleanup_mount(new_root);
        child_fail("fchdir(newroot) failed");
    }

    if (do_pivot_root(".", ".") != 0) {
        close(newroot_fd);
        close(oldroot_fd);
        child_fail(strerror(errno));
    }

    if (fchdir(oldroot_fd) != 0) {
        close(newroot_fd);
        close(oldroot_fd);
        child_fail("fchdir(oldroot) failed after pivot");
    }

    if (umount2(".", MNT_DETACH) != 0) {
        close(newroot_fd);
        close(oldroot_fd);
        child_fail("umount2(oldroot) failed");
    }

    if (chdir("/") != 0) {
        close(newroot_fd);
        close(oldroot_fd);
        child_fail("chdir(/) failed after detach");
    }

    if (getcwd(cwd, sizeof(cwd)) == nullptr || strcmp(cwd, "/") != 0) {
        close(newroot_fd);
        close(oldroot_fd);
        child_fail("cwd is not / after dot-dot pivot");
    }

    if (access("/bin", F_OK) != 0) {
        close(newroot_fd);
        close(oldroot_fd);
        child_fail("new root is not visible after dot-dot pivot");
    }

    close(newroot_fd);
    close(oldroot_fd);
    child_pass();
}

void case_dot_dot_rslave_detach() {
    const char* new_root = "/tmp/test_pivot_root/dotdot_rslave/newroot";
    const char* bin_dir = "/tmp/test_pivot_root/dotdot_rslave/newroot/bin";
    int oldroot_fd = -1;
    int newroot_fd = -1;

    ensure_parent_tree();
    ensure_dir("/tmp/test_pivot_root/dotdot_rslave");
    ensure_dir(new_root);
    prepare_private_mount_namespace();

    if (mount("", new_root, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }

    if (mkdir(bin_dir, 0755) != 0 && errno != EEXIST) {
        cleanup_mount(new_root);
        child_fail("mkdir(bin) failed");
    }

    oldroot_fd = open("/", O_DIRECTORY | O_RDONLY);
    if (oldroot_fd < 0) {
        cleanup_mount(new_root);
        child_fail("open(oldroot) failed");
    }

    newroot_fd = open(new_root, O_DIRECTORY | O_RDONLY);
    if (newroot_fd < 0) {
        close(oldroot_fd);
        cleanup_mount(new_root);
        child_fail("open(newroot) failed");
    }

    if (fchdir(newroot_fd) != 0) {
        close(newroot_fd);
        close(oldroot_fd);
        cleanup_mount(new_root);
        child_fail("fchdir(newroot) failed");
    }

    if (do_pivot_root(".", ".") != 0) {
        close(newroot_fd);
        close(oldroot_fd);
        child_fail(strerror(errno));
    }

    if (fchdir(oldroot_fd) != 0) {
        close(newroot_fd);
        close(oldroot_fd);
        child_fail("fchdir(oldroot) failed after pivot");
    }

    if (mount(nullptr, ".", nullptr, MS_REC | MS_SLAVE, nullptr) != 0) {
        close(newroot_fd);
        close(oldroot_fd);
        child_fail("mount(make-rslave) failed");
    }

    if (umount2(".", MNT_DETACH) != 0) {
        close(newroot_fd);
        close(oldroot_fd);
        child_fail("umount2(oldroot) failed after make-rslave");
    }

    close(newroot_fd);
    close(oldroot_fd);
    child_pass();
}

void case_new_root_not_mountpoint() {
    const char* new_root = "/tmp/test_pivot_root/not_mountpoint/newroot";
    const char* oldroot_abs = "/tmp/test_pivot_root/not_mountpoint/newroot/oldroot";

    ensure_parent_tree();
    ensure_dir("/tmp/test_pivot_root/not_mountpoint");
    ensure_dir(new_root);
    ensure_dir(oldroot_abs);
    prepare_private_mount_namespace();

    if (do_pivot_root(new_root, oldroot_abs) == -1 && errno == EINVAL) {
        child_pass();
    }

    child_fail("expected EINVAL");
}

void case_put_old_outside_new_root() {
    const char* new_root = "/tmp/test_pivot_root/put_old_outside/newroot";
    const char* outside = "/tmp/test_pivot_root/put_old_outside/outside";
    const char* inside = "/tmp/test_pivot_root/put_old_outside/newroot/inside";

    ensure_parent_tree();
    ensure_dir("/tmp/test_pivot_root/put_old_outside");
    ensure_dir(new_root);
    ensure_dir(outside);
    prepare_private_mount_namespace();

    if (mount("", new_root, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }

    if (mkdir(inside, 0755) != 0 && errno != EEXIST) {
        cleanup_mount(new_root);
        child_fail("mkdir(inside) failed");
    }

    if (do_pivot_root(new_root, outside) == -1 && errno == EINVAL) {
        cleanup_mount(new_root);
        child_pass();
    }

    cleanup_mount(new_root);
    child_fail("expected EINVAL");
}

void case_busy_target() {
    const char* new_root = "/tmp/test_pivot_root/busy/newroot";
    const char* oldroot_abs = "/tmp/test_pivot_root/busy/newroot/oldroot";

    ensure_parent_tree();
    ensure_dir("/tmp/test_pivot_root/busy");
    ensure_dir(new_root);
    prepare_private_mount_namespace();

    if (mount("", new_root, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }

    if (mkdir(oldroot_abs, 0755) != 0 && errno != EEXIST) {
        cleanup_mount(new_root);
        child_fail("mkdir(oldroot) failed");
    }

    if (do_pivot_root(new_root, "/") == -1 && errno == EBUSY) {
        cleanup_mount(new_root);
        child_pass();
    }

    cleanup_mount(new_root);
    child_fail("expected EBUSY");
}

void case_permission_failure() {
    const char* new_root = "/tmp/test_pivot_root/perm/newroot";
    const char* oldroot_abs = "/tmp/test_pivot_root/perm/newroot/oldroot";

    ensure_parent_tree();
    ensure_dir("/tmp/test_pivot_root/perm");
    ensure_dir(new_root);
    prepare_private_mount_namespace();

    if (mount("", new_root, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }

    if (mkdir(oldroot_abs, 0755) != 0 && errno != EEXIST) {
        cleanup_mount(new_root);
        child_fail("mkdir(oldroot) failed");
    }

    if (seteuid(65534) != 0) {
        cleanup_mount(new_root);
        child_skip("seteuid failed");
    }

    if (do_pivot_root(new_root, oldroot_abs) == -1 && errno == EPERM) {
        child_pass();
    }

    child_fail("expected EPERM");
}

// ---- Tests for shared mount rejection ----

void case_shared_mount_rejection() {
    const char* new_root = "/tmp/test_pivot_root/shared/newroot";
    const char* oldroot_abs = "/tmp/test_pivot_root/shared/newroot/oldroot";

    ensure_parent_tree();
    ensure_dir("/tmp/test_pivot_root/shared");
    ensure_dir(new_root);

    // Enter new mount namespace but DON'T make it private — keep shared
    if (unshare(CLONE_NEWNS) != 0) {
        child_skip(strerror(errno));
    }

    // Explicitly make root shared
    if (mount(nullptr, "/", nullptr, MS_REC | MS_SHARED, nullptr) != 0) {
        child_skip("cannot make root shared");
    }

    if (mount("", new_root, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }

    if (mkdir(oldroot_abs, 0755) != 0 && errno != EEXIST) {
        cleanup_mount(new_root);
        child_fail("mkdir(oldroot) failed");
    }

    // pivot_root should fail with EINVAL because mounts are shared
    if (do_pivot_root(new_root, oldroot_abs) == -1 && errno == EINVAL) {
        cleanup_mount(new_root);
        child_pass();
    }

    cleanup_mount(new_root);
    child_fail("expected EINVAL for shared mount");
}

// ---- Test for mount namespace isolation (BUG-0a regression) ----
// After clone(CLONE_NEWNS), child's mounts must NOT leak into parent namespace.

void case_mount_namespace_isolation() {
    const char* marker = "/tmp/test_pivot_root/ns_isolation_marker";

    ensure_parent_tree();

    // Remove marker if it exists from a previous run
    rmdir(marker);

    pid_t child = fork();
    if (child < 0) {
        child_fail("fork failed");
    }

    if (child == 0) {
        // Child: create new mount namespace, mount tmpfs, create marker dir
        if (unshare(CLONE_NEWNS) != 0) {
            _exit(77);  // skip
        }

        if (mount(nullptr, "/", nullptr, MS_REC | MS_PRIVATE, nullptr) != 0) {
            _exit(77);
        }

        ensure_dir("/tmp/test_pivot_root/ns_iso");

        if (mount("tmpfs", "/tmp/test_pivot_root/ns_iso", "tmpfs", 0, nullptr) != 0) {
            _exit(77);
        }

        // Create a marker directory inside the tmpfs
        mkdir("/tmp/test_pivot_root/ns_iso/child_was_here", 0755);

        // Verify marker exists in child
        struct stat st = {};
        if (stat("/tmp/test_pivot_root/ns_iso/child_was_here", &st) != 0) {
            _exit(2);  // fail: can't even see own marker
        }

        umount("/tmp/test_pivot_root/ns_iso");
        _exit(0);
    }

    int status = 0;
    waitpid(child, &status, 0);

    if (!WIFEXITED(status)) {
        child_fail("child did not exit normally");
    }

    int code = WEXITSTATUS(status);
    if (code == 77) {
        child_skip("child could not set up namespace");
    }
    if (code == 2) {
        child_fail("child could not see its own marker");
    }
    if (code != 0) {
        child_fail("child exited with unexpected code");
    }

    // Parent: check that child's tmpfs mount is NOT visible here.
    // If namespace isolation works, /tmp/test_pivot_root/ns_iso/child_was_here
    // should NOT exist in the parent namespace.
    struct stat st = {};
    if (stat("/tmp/test_pivot_root/ns_iso/child_was_here", &st) == 0) {
        child_fail("child's tmpfs mount leaked into parent namespace (BUG-0a)");
    }

    child_pass();
}

// ---- Test for double pivot_root (pivot then pivot back) ----

void case_double_pivot() {
    const char* new_root = "/tmp/test_pivot_root/double/newroot";
    const char* oldroot_abs = "/tmp/test_pivot_root/double/newroot/oldroot";

    ensure_parent_tree();
    ensure_dir("/tmp/test_pivot_root/double");
    ensure_dir(new_root);
    prepare_private_mount_namespace();

    if (mount("", new_root, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }

    if (mkdir(oldroot_abs, 0755) != 0 && errno != EEXIST) {
        cleanup_mount(new_root);
        child_fail("mkdir(oldroot) failed");
    }

    if (chdir(new_root) != 0) {
        cleanup_mount(new_root);
        child_fail("chdir(new_root) failed");
    }

    // First pivot: make ramfs the new root
    if (do_pivot_root(".", "oldroot") != 0) {
        child_fail("first pivot_root failed");
    }

    chdir("/");

    // Verify first pivot worked
    if (access("/oldroot", F_OK) != 0) {
        child_fail("old root not accessible at /oldroot after first pivot");
    }

    // Create put_back directory inside old root for second pivot
    if (mkdir("/oldroot/put_back", 0755) != 0 && errno != EEXIST) {
        child_fail("mkdir(put_back) in oldroot failed");
    }

    // Second pivot: restore old root
    if (do_pivot_root("/oldroot", "/oldroot/put_back") != 0) {
        child_fail(strerror(errno));
    }

    chdir("/");

    // After pivoting back, /put_back should contain the ramfs
    if (access("/put_back", F_OK) != 0) {
        child_fail("/put_back not accessible after second pivot");
    }

    child_pass();
}

TEST(PivotRoot, SuccessPath) {
    expect_case_pass_or_skip("pivot_root_success", case_success_path);
}

TEST(PivotRoot, DotDotPath) {
    expect_case_pass_or_skip("pivot_root_dot_dot", case_dot_dot_path);
}

TEST(PivotRoot, DotDotRslaveDetach) {
    expect_case_pass_or_skip("pivot_root_dot_dot_rslave_detach", case_dot_dot_rslave_detach);
}

TEST(PivotRoot, NewRootNotMountpoint) {
    expect_case_pass_or_skip("pivot_root_new_root_not_mountpoint", case_new_root_not_mountpoint);
}

TEST(PivotRoot, PutOldOutsideNewRoot) {
    expect_case_pass_or_skip("pivot_root_put_old_outside_new_root", case_put_old_outside_new_root);
}

TEST(PivotRoot, BusyTarget) {
    expect_case_pass_or_skip("pivot_root_busy_target", case_busy_target);
}

TEST(PivotRoot, PermissionFailure) {
    expect_case_pass_or_skip("pivot_root_permission_failure", case_permission_failure);
}

TEST(PivotRoot, SharedMountRejection) {
    expect_case_pass_or_skip("pivot_root_shared_mount_rejection", case_shared_mount_rejection);
}

TEST(PivotRoot, MountNamespaceIsolation) {
    expect_case_pass_or_skip("pivot_root_mount_namespace_isolation",
                             case_mount_namespace_isolation);
}

TEST(PivotRoot, DoublePivot) {
    expect_case_pass_or_skip("pivot_root_double_pivot", case_double_pivot);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
