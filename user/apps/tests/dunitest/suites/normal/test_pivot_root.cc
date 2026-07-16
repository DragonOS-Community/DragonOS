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
    const char* old_root = "/tmp/test_pivot_root/success/root";
    const char* new_root = "/tmp/test_pivot_root/success/root/newroot";
    const char* put_old = "oldroot";
    const char* oldroot_abs = "/tmp/test_pivot_root/success/root/newroot/oldroot";
    const char* bin_dir = "/tmp/test_pivot_root/success/root/newroot/bin";
    char cwd[256] = {};

    ensure_parent_tree();
    ensure_dir("/tmp/test_pivot_root/success");
    ensure_dir(old_root);
    prepare_private_mount_namespace();

    if (mount("", old_root, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }
    if (mkdir(new_root, 0755) != 0 && errno != EEXIST) {
        child_fail("mkdir(newroot) failed");
    }
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

    if (chroot(old_root) != 0 || chdir("/newroot") != 0) {
        child_fail("chroot/chdir(new_root) failed");
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
    const char* old_root = "/tmp/test_pivot_root/dotdot/root";
    const char* new_root = "/tmp/test_pivot_root/dotdot/root/newroot";
    const char* bin_dir = "/tmp/test_pivot_root/dotdot/root/newroot/bin";
    int oldroot_fd = -1;
    int newroot_fd = -1;
    char cwd[256] = {};

    ensure_parent_tree();
    ensure_dir("/tmp/test_pivot_root/dotdot");
    ensure_dir(old_root);
    prepare_private_mount_namespace();

    if (mount("", old_root, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }
    if (mkdir(new_root, 0755) != 0 && errno != EEXIST) {
        child_fail("mkdir(newroot) failed");
    }
    if (mount("", new_root, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }

    if (mkdir(bin_dir, 0755) != 0 && errno != EEXIST) {
        cleanup_mount(new_root);
        child_fail("mkdir(bin) failed");
    }

    if (chroot(old_root) != 0 || chdir("/") != 0) {
        child_fail("chroot(old_root) failed");
    }

    oldroot_fd = open("/", O_DIRECTORY | O_RDONLY);
    if (oldroot_fd < 0) {
        cleanup_mount(new_root);
        child_fail("open(oldroot) failed");
    }

    newroot_fd = open("/newroot", O_DIRECTORY | O_RDONLY);
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
    const char* old_root = "/tmp/test_pivot_root/dotdot_rslave/root";
    const char* new_root = "/tmp/test_pivot_root/dotdot_rslave/root/newroot";
    const char* bin_dir = "/tmp/test_pivot_root/dotdot_rslave/root/newroot/bin";
    int oldroot_fd = -1;
    int newroot_fd = -1;

    ensure_parent_tree();
    ensure_dir("/tmp/test_pivot_root/dotdot_rslave");
    ensure_dir(old_root);
    prepare_private_mount_namespace();

    if (mount("", old_root, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }
    if (mkdir(new_root, 0755) != 0 && errno != EEXIST) {
        child_fail("mkdir(newroot) failed");
    }
    if (mount("", new_root, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }

    if (mkdir(bin_dir, 0755) != 0 && errno != EEXIST) {
        cleanup_mount(new_root);
        child_fail("mkdir(bin) failed");
    }

    if (chroot(old_root) != 0 || chdir("/") != 0) {
        child_fail("chroot(old_root) failed");
    }

    oldroot_fd = open("/", O_DIRECTORY | O_RDONLY);
    if (oldroot_fd < 0) {
        cleanup_mount(new_root);
        child_fail("open(oldroot) failed");
    }

    newroot_fd = open("/newroot", O_DIRECTORY | O_RDONLY);
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
    const char* old_root = "/tmp/test_pivot_root/not_mountpoint/root";
    const char* containing_mount = "/tmp/test_pivot_root/not_mountpoint/root/mount";
    const char* new_root = "/tmp/test_pivot_root/not_mountpoint/root/mount/newroot";
    const char* oldroot_abs =
        "/tmp/test_pivot_root/not_mountpoint/root/mount/newroot/oldroot";

    ensure_parent_tree();
    ensure_dir("/tmp/test_pivot_root/not_mountpoint");
    ensure_dir(old_root);
    prepare_private_mount_namespace();

    if (mount("", old_root, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }
    if (mkdir(containing_mount, 0755) != 0 && errno != EEXIST) {
        child_fail("mkdir(containing mount) failed");
    }
    if (mount("", containing_mount, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }
    if ((mkdir(new_root, 0755) != 0 && errno != EEXIST) ||
        (mkdir(oldroot_abs, 0755) != 0 && errno != EEXIST)) {
        child_fail("mkdir non-mount newroot layout failed");
    }
    if (chroot(old_root) != 0 || chdir("/") != 0) {
        child_fail("chroot(old_root) failed");
    }

    if (do_pivot_root("/mount/newroot", "/mount/newroot/oldroot") == -1 && errno == EINVAL) {
        child_pass();
    }

    child_fail("expected EINVAL");
}

void case_put_old_outside_new_root() {
    const char* old_root = "/tmp/test_pivot_root/put_old_outside/root";
    const char* new_root = "/tmp/test_pivot_root/put_old_outside/root/newroot";
    const char* outside = "/tmp/test_pivot_root/put_old_outside/root/outside";
    const char* inside = "/tmp/test_pivot_root/put_old_outside/root/newroot/inside";

    ensure_parent_tree();
    ensure_dir("/tmp/test_pivot_root/put_old_outside");
    ensure_dir(old_root);
    prepare_private_mount_namespace();

    if (mount("", old_root, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }
    if ((mkdir(new_root, 0755) != 0 && errno != EEXIST) ||
        (mkdir(outside, 0755) != 0 && errno != EEXIST)) {
        child_fail("mkdir reachability layout failed");
    }
    if (mount("", new_root, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }
    if (mount("", outside, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }

    if (mkdir(inside, 0755) != 0 && errno != EEXIST) {
        child_fail("mkdir(inside) failed");
    }
    if (chroot(old_root) != 0 || chdir("/") != 0) {
        child_fail("chroot(old_root) failed");
    }

    if (do_pivot_root("/newroot", "/outside") == -1 && errno == EINVAL) {
        child_pass();
    }

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
    const char* old_root = "/tmp/test_pivot_root/shared/root";
    const char* new_root = "/tmp/test_pivot_root/shared/root/newroot";
    const char* oldroot_abs = "/tmp/test_pivot_root/shared/root/newroot/oldroot";

    ensure_parent_tree();
    ensure_dir("/tmp/test_pivot_root/shared");
    ensure_dir(old_root);
    prepare_private_mount_namespace();
    if (mount("", old_root, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }
    if (mkdir(new_root, 0755) != 0 && errno != EEXIST) {
        child_fail("mkdir(newroot) failed");
    }

    if (mount("", new_root, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }

    if (mkdir(oldroot_abs, 0755) != 0 && errno != EEXIST) {
        cleanup_mount(new_root);
        child_fail("mkdir(oldroot) failed");
    }
    // put_old resolves on new_root, so making this mount shared exercises
    // Linux's IS_MNT_SHARED(old_mnt) rejection without an unattached root.
    if (mount(nullptr, new_root, nullptr, MS_SHARED, nullptr) != 0) {
        child_skip("cannot make put_old mount shared");
    }
    if (chroot(old_root) != 0 || chdir("/") != 0) {
        child_fail("chroot(old_root) failed");
    }

    if (do_pivot_root("/newroot", "/newroot/oldroot") == -1 && errno == EINVAL) {
        child_pass();
    }

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
    const char* old_root = "/tmp/test_pivot_root/double/root";
    const char* new_root = "/tmp/test_pivot_root/double/root/newroot";
    const char* oldroot_abs = "/tmp/test_pivot_root/double/root/newroot/oldroot";

    ensure_parent_tree();
    ensure_dir("/tmp/test_pivot_root/double");
    ensure_dir(old_root);
    prepare_private_mount_namespace();

    if (mount("", old_root, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }
    if (mkdir(new_root, 0755) != 0 && errno != EEXIST) {
        child_fail("mkdir(newroot) failed");
    }
    if (mount("", new_root, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }

    if (mkdir(oldroot_abs, 0755) != 0 && errno != EEXIST) {
        cleanup_mount(new_root);
        child_fail("mkdir(oldroot) failed");
    }

    if (chroot(old_root) != 0 || chdir("/newroot") != 0) {
        child_fail("chroot/chdir(new_root) failed");
    }

    // First pivot: make ramfs the new root
    if (do_pivot_root(".", "oldroot") != 0) {
        child_fail("first pivot_root failed");
    }

    if (chdir("/") != 0) {
        child_fail("chdir(/) failed after first pivot");
    }

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

    if (chdir("/") != 0) {
        child_fail("chdir(/) failed after second pivot");
    }

    // After pivoting back, /put_back should contain the ramfs
    if (access("/put_back", F_OK) != 0) {
        child_fail("/put_back not accessible after second pivot");
    }

    child_pass();
}

void case_chroot_mount_root() {
    const char* sandbox = "/tmp/test_pivot_root/chroot_mount";
    const char* new_root = "/tmp/test_pivot_root/chroot_mount/newroot";
    const char* old_root = "/tmp/test_pivot_root/chroot_mount/newroot/oldroot";

    ensure_parent_tree();
    ensure_dir(sandbox);
    prepare_private_mount_namespace();
    if (mount("", sandbox, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }
    if (mkdir(new_root, 0755) != 0 && errno != EEXIST) {
        child_fail("mkdir(newroot) failed");
    }
    if (mount("", new_root, "ramfs", 0, nullptr) != 0) {
        child_fail("mount(newroot) failed");
    }
    if (mkdir(old_root, 0755) != 0 && errno != EEXIST) {
        child_fail("mkdir(oldroot) failed");
    }
    if (chroot(sandbox) != 0 || chdir("/") != 0) {
        child_fail("chroot to mounted sandbox failed");
    }
    if (do_pivot_root("/newroot", "/newroot/oldroot") != 0) {
        child_fail(strerror(errno));
    }
    if (access("/oldroot", F_OK) != 0) {
        child_fail("nested current root not attached below put_old");
    }
    child_pass();
}

void case_chroot_ordinary_directory_rejected() {
    const char* sandbox = "/tmp/test_pivot_root/chroot_ordinary";
    const char* jail = "/tmp/test_pivot_root/chroot_ordinary/jail";
    const char* new_root = "/tmp/test_pivot_root/chroot_ordinary/jail/newroot";
    const char* old_root = "/tmp/test_pivot_root/chroot_ordinary/jail/newroot/oldroot";

    ensure_parent_tree();
    ensure_dir(sandbox);
    prepare_private_mount_namespace();
    if (mount("", sandbox, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }
    if (mkdir(jail, 0755) != 0 && errno != EEXIST) {
        child_fail("mkdir(jail) failed");
    }
    if (mkdir(new_root, 0755) != 0 && errno != EEXIST) {
        child_fail("mkdir(newroot) failed");
    }
    if (mount("", new_root, "ramfs", 0, nullptr) != 0) {
        child_fail("mount(newroot) failed");
    }
    if (mkdir(old_root, 0755) != 0 && errno != EEXIST) {
        child_fail("mkdir(oldroot) failed");
    }
    if (chroot(jail) != 0 || chdir("/") != 0) {
        child_fail("chroot to ordinary directory failed");
    }
    errno = 0;
    if (do_pivot_root("/newroot", "/newroot/oldroot") != -1 || errno != EINVAL) {
        child_fail("pivot_root from ordinary chroot did not return EINVAL");
    }
    child_pass();
}

void case_namespace_root_rejected() {
    const char* base = "/tmp/test_pivot_root/namespace_root";
    const char* new_root = "/tmp/test_pivot_root/namespace_root/newroot";
    const char* put_old = "/tmp/test_pivot_root/namespace_root/newroot/oldroot";

    ensure_parent_tree();
    ensure_dir(base);
    ensure_dir(new_root);
    prepare_private_mount_namespace();
    if (mount("", new_root, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }
    if (mkdir(put_old, 0755) != 0 && errno != EEXIST) {
        child_fail("mkdir(oldroot) failed");
    }

    errno = 0;
    if (do_pivot_root(new_root, put_old) != -1 || errno != EINVAL) {
        child_fail("pivot_root from namespace root did not return EINVAL");
    }
    child_pass();
}

void case_new_root_on_root_mount() {
    const char* old_root = "/tmp/test_pivot_root/new_on_root/root";
    const char* new_root = "/tmp/test_pivot_root/new_on_root/root/newroot";
    const char* put_old = "/tmp/test_pivot_root/new_on_root/root/newroot/oldroot";

    ensure_parent_tree();
    ensure_dir("/tmp/test_pivot_root/new_on_root");
    ensure_dir(old_root);
    prepare_private_mount_namespace();
    if (mount("", old_root, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }
    if (mkdir(new_root, 0755) != 0 && errno != EEXIST) {
        child_fail("mkdir(newroot) failed");
    }
    if (mkdir(put_old, 0755) != 0 && errno != EEXIST) {
        child_fail("mkdir(oldroot) failed");
    }
    if (chroot(old_root) != 0 || chdir("/") != 0) {
        child_fail("chroot(old_root) failed");
    }

    errno = 0;
    if (do_pivot_root("/newroot", "/newroot/oldroot") != -1 || errno != EBUSY) {
        child_fail("new_root on current root mount did not return EBUSY");
    }
    child_pass();
}

void case_unreachable_new_root() {
    const char* outside_root = "/tmp/test_pivot_root/unreachable/outside";
    const char* old_root = "/tmp/test_pivot_root/unreachable/outside/root";
    const char* new_root = "/tmp/test_pivot_root/unreachable/outside/root/newroot";
    const char* put_old = "/tmp/test_pivot_root/unreachable/outside/root/newroot/oldroot";

    ensure_parent_tree();
    ensure_dir("/tmp/test_pivot_root/unreachable");
    ensure_dir(outside_root);
    prepare_private_mount_namespace();
    if (mount("", outside_root, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }
    if (mkdir(old_root, 0755) != 0 && errno != EEXIST) {
        child_fail("mkdir(root) failed");
    }
    if (mount("", old_root, "ramfs", 0, nullptr) != 0) {
        child_fail("mount(root) failed");
    }
    if (mkdir(new_root, 0755) != 0 && errno != EEXIST) {
        child_fail("mkdir(newroot) failed");
    }
    if (mount("", new_root, "ramfs", 0, nullptr) != 0) {
        child_fail("mount(newroot) failed");
    }
    if (mkdir(put_old, 0755) != 0 && errno != EEXIST) {
        child_fail("mkdir(oldroot) failed");
    }
    if (chdir(outside_root) != 0 || chroot(old_root) != 0) {
        child_fail("prepare cwd outside chroot failed");
    }

    errno = 0;
    if (do_pivot_root(".", "/newroot/oldroot") != -1 || errno != EINVAL) {
        child_fail("unreachable new_root did not return EINVAL");
    }
    child_pass();
}

void case_nested_pivot_preserves_upper_sibling() {
    const char* base = "/tmp/test_pivot_root/nested_topology";
    const char* old_root = "/tmp/test_pivot_root/nested_topology/root";
    const char* new_root = "/tmp/test_pivot_root/nested_topology/root/newroot";
    const char* put_old = "/tmp/test_pivot_root/nested_topology/root/newroot/oldroot";
    const char* sibling = "/tmp/test_pivot_root/nested_topology/sibling";
    const char* marker = "/tmp/test_pivot_root/nested_topology/sibling/marker";
    struct stat before = {};
    struct stat after = {};

    ensure_parent_tree();
    ensure_dir(base);
    ensure_dir(old_root);
    ensure_dir(sibling);
    prepare_private_mount_namespace();
    if (mount("", old_root, "ramfs", 0, nullptr) != 0 ||
        mount("", sibling, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }
    if (mkdir(new_root, 0755) != 0 && errno != EEXIST) {
        child_fail("mkdir(newroot) failed");
    }
    if (mount("", new_root, "ramfs", 0, nullptr) != 0) {
        child_fail("mount(newroot) failed");
    }
    if ((mkdir(put_old, 0755) != 0 && errno != EEXIST) ||
        (mkdir(marker, 0755) != 0 && errno != EEXIST) || stat(marker, &before) != 0) {
        child_fail("prepare topology marker failed");
    }

    pid_t pivot_child = fork();
    if (pivot_child < 0) {
        child_fail("fork pivot child failed");
    }
    if (pivot_child == 0) {
        if (chroot(old_root) != 0 || chdir("/") != 0) {
            _exit(2);
        }
        if (do_pivot_root("/newroot", "/newroot/oldroot") != 0) {
            _exit(3);
        }
        if (access("/oldroot", F_OK) != 0) {
            _exit(4);
        }
        _exit(0);
    }

    int status = 0;
    if (waitpid(pivot_child, &status, 0) != pivot_child || !WIFEXITED(status) ||
        WEXITSTATUS(status) != 0) {
        child_fail("nested pivot child failed");
    }
    if (stat(marker, &after) != 0 || before.st_dev != after.st_dev || before.st_ino != after.st_ino) {
        child_fail("pivot moved or replaced an upper sibling mount");
    }
    child_pass();
}

void case_private_stacked_put_old() {
    const char* old_root = "/tmp/test_pivot_root/stacked/root";
    const char* new_root = "/tmp/test_pivot_root/stacked/root/newroot";
    const char* put_old = "/tmp/test_pivot_root/stacked/root/newroot/oldroot";
    const char* covered_marker = "/tmp/test_pivot_root/stacked/root/newroot/oldroot/marker";

    ensure_parent_tree();
    ensure_dir("/tmp/test_pivot_root/stacked");
    ensure_dir(old_root);
    prepare_private_mount_namespace();
    if (mount("", old_root, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }
    if (mkdir(new_root, 0755) != 0 && errno != EEXIST) {
        child_fail("mkdir(newroot) failed");
    }
    if (mount("", new_root, "ramfs", 0, nullptr) != 0) {
        child_fail("mount(newroot) failed");
    }
    if (mkdir(put_old, 0755) != 0 && errno != EEXIST) {
        child_fail("mkdir(oldroot) failed");
    }
    if (mount("", put_old, "ramfs", 0, nullptr) != 0) {
        child_skip("private put_old mount is unavailable");
    }
    if (mkdir(covered_marker, 0755) != 0 && errno != EEXIST) {
        child_fail("mkdir covered marker failed");
    }
    if (chroot(old_root) != 0 || chdir("/") != 0) {
        child_fail("chroot(old_root) failed");
    }
    if (do_pivot_root("/newroot", "/newroot/oldroot") != 0) {
        child_fail("pivot_root onto stacked put_old failed");
    }
    if (access("/oldroot/marker", F_OK) == 0) {
        child_fail("old root did not cover the existing put_old mount");
    }
    if (umount("/oldroot") != 0 || access("/oldroot/marker", F_OK) != 0) {
        child_fail("existing put_old mount was not revealed after umount");
    }
    child_pass();
}

void case_cross_process_exact_fs_refs() {
    const char* base = "/tmp/test_pivot_root/fs_refs";
    const char* old_root = "/tmp/test_pivot_root/fs_refs/root";
    const char* new_root = "/tmp/test_pivot_root/fs_refs/root/newroot";
    const char* put_old = "/tmp/test_pivot_root/fs_refs/root/newroot/oldroot";
    const char* sibling = "/tmp/test_pivot_root/fs_refs/sibling";
    const char* outside_marker = "/tmp/test_pivot_root/fs_refs/outside_marker";
    struct stat expected_new = {};
    struct stat sibling_before = {};
    int ready[2] = {-1, -1};
    int release[2] = {-1, -1};

    ensure_parent_tree();
    ensure_dir(base);
    ensure_dir(old_root);
    ensure_dir(sibling);
    ensure_dir(outside_marker);
    prepare_private_mount_namespace();
    if (mount("", old_root, "ramfs", 0, nullptr) != 0 ||
        mount("", sibling, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }
    if (mkdir(new_root, 0755) != 0 && errno != EEXIST) {
        child_fail("mkdir(newroot) failed");
    }
    if (mount("", new_root, "ramfs", 0, nullptr) != 0) {
        child_fail("mount(newroot) failed");
    }
    if (mkdir(put_old, 0755) != 0 && errno != EEXIST) {
        child_fail("mkdir(oldroot) failed");
    }
    if (stat(new_root, &expected_new) != 0 || chdir(sibling) != 0 ||
        stat(".", &sibling_before) != 0) {
        child_fail("prepare fs-ref identities failed");
    }
    if (pipe(ready) != 0 || pipe(release) != 0) {
        child_fail("pipe fs-ref barrier failed");
    }

    pid_t watcher = fork();
    if (watcher < 0) {
        child_fail("fork fs-ref watcher failed");
    }
    if (watcher == 0) {
        close(ready[0]);
        close(release[1]);
        if (chroot(old_root) != 0 || chdir("/") != 0) {
            _exit(2);
        }
        char token = 'R';
        if (write(ready[1], &token, 1) != 1 || read(release[0], &token, 1) != 1) {
            _exit(3);
        }
        struct stat root_after = {};
        struct stat pwd_after = {};
        if (stat("/", &root_after) != 0 || stat(".", &pwd_after) != 0) {
            _exit(4);
        }
        if (root_after.st_dev != expected_new.st_dev || root_after.st_ino != expected_new.st_ino ||
            pwd_after.st_dev != expected_new.st_dev || pwd_after.st_ino != expected_new.st_ino) {
            _exit(5);
        }
        _exit(0);
    }

    close(ready[1]);
    close(release[0]);
    char token = 0;
    if (read(ready[0], &token, 1) != 1 || token != 'R') {
        child_fail("fs-ref watcher did not reach barrier");
    }

    pid_t pivot_child = fork();
    if (pivot_child < 0) {
        child_fail("fork fs-ref pivot child failed");
    }
    if (pivot_child == 0) {
        close(ready[0]);
        close(release[1]);
        if (chroot(old_root) != 0 || chdir("/") != 0) {
            _exit(6);
        }
        if (do_pivot_root("/newroot", "/newroot/oldroot") != 0) {
            _exit(7);
        }
        _exit(0);
    }

    int pivot_status = 0;
    if (waitpid(pivot_child, &pivot_status, 0) != pivot_child || !WIFEXITED(pivot_status) ||
        WEXITSTATUS(pivot_status) != 0) {
        child_fail("fs-ref pivot child failed");
    }

    struct stat sibling_after = {};
    if (stat(".", &sibling_after) != 0 || sibling_before.st_dev != sibling_after.st_dev ||
        sibling_before.st_ino != sibling_after.st_ino || access(outside_marker, F_OK) != 0) {
        child_fail("non-matching fs root or pwd was replaced");
    }

    token = 'G';
    if (write(release[1], &token, 1) != 1) {
        child_fail("release fs-ref watcher failed");
    }
    int watcher_status = 0;
    if (waitpid(watcher, &watcher_status, 0) != watcher || !WIFEXITED(watcher_status) ||
        WEXITSTATUS(watcher_status) != 0) {
        child_fail("matching fs root/pwd were not replaced");
    }
    child_pass();
}

void case_symlink_to_mounted_new_root() {
    const char* old_root = "/tmp/test_pivot_root/symlink/root";
    const char* new_root = "/tmp/test_pivot_root/symlink/root/newroot";
    const char* new_root_link = "/tmp/test_pivot_root/symlink/root/newroot_link";
    const char* put_old = "/tmp/test_pivot_root/symlink/root/newroot/oldroot";
    const char* marker = "/tmp/test_pivot_root/symlink/root/newroot/marker";

    ensure_parent_tree();
    ensure_dir("/tmp/test_pivot_root/symlink");
    ensure_dir(old_root);
    prepare_private_mount_namespace();
    if (mount("", old_root, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }
    if (mkdir(new_root, 0755) != 0 && errno != EEXIST) {
        child_fail("mkdir(newroot) failed");
    }
    if (mount("", new_root, "ramfs", 0, nullptr) != 0) {
        child_fail("mount(newroot) failed");
    }
    if ((mkdir(put_old, 0755) != 0 && errno != EEXIST) ||
        (mkdir(marker, 0755) != 0 && errno != EEXIST)) {
        child_fail("prepare symlink new-root contents failed");
    }
    if (symlink("newroot", new_root_link) != 0) {
        child_fail("symlink(newroot) failed");
    }
    if (chroot(old_root) != 0 || chdir("/") != 0) {
        child_fail("chroot(old_root) failed");
    }
    if (do_pivot_root("/newroot_link", "/newroot_link/oldroot") != 0) {
        child_fail("pivot_root through final symlink failed");
    }
    if (access("/oldroot", F_OK) != 0 || access("/marker", F_OK) != 0) {
        child_fail("symlink pivot did not install new or old root");
    }
    child_pass();
}

void case_self_bind_alias_new_root() {
    const char* old_root = "/tmp/test_pivot_root/bind_alias/root";
    const char* candidate = "/tmp/test_pivot_root/bind_alias/root/candidate";
    const char* put_old = "/tmp/test_pivot_root/bind_alias/root/candidate/oldroot";
    const char* marker = "/tmp/test_pivot_root/bind_alias/root/candidate/marker";

    ensure_parent_tree();
    ensure_dir("/tmp/test_pivot_root/bind_alias");
    ensure_dir(old_root);
    prepare_private_mount_namespace();
    if (mount("", old_root, "ramfs", 0, nullptr) != 0) {
        child_skip(strerror(errno));
    }
    if (mkdir(candidate, 0755) != 0 && errno != EEXIST) {
        child_fail("mkdir(bind candidate) failed");
    }
    if ((mkdir(put_old, 0755) != 0 && errno != EEXIST) ||
        (mkdir(marker, 0755) != 0 && errno != EEXIST)) {
        child_fail("prepare bind candidate contents failed");
    }
    // A self-bind gives the same backing dentry a distinct mount identity,
    // making an ordinary directory a legal pivot_root new_root.
    if (mount(candidate, candidate, nullptr, MS_BIND, nullptr) != 0) {
        child_skip("self-bind mount is unavailable");
    }
    if (chroot(old_root) != 0 || chdir("/") != 0) {
        child_fail("chroot(old_root) failed");
    }
    if (do_pivot_root("/candidate", "/candidate/oldroot") != 0) {
        child_fail("pivot_root to self-bind alias failed");
    }
    if (access("/oldroot", F_OK) != 0 || access("/marker", F_OK) != 0) {
        child_fail("bind-alias pivot did not install new or old root");
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

TEST(PivotRoot, ChrootMountRoot) {
    expect_case_pass_or_skip("pivot_root_chroot_mount_root", case_chroot_mount_root);
}

TEST(PivotRoot, ChrootOrdinaryDirectoryRejected) {
    expect_case_pass_or_skip("pivot_root_chroot_ordinary_directory_rejected",
                             case_chroot_ordinary_directory_rejected);
}

TEST(PivotRoot, NamespaceRootRejected) {
    expect_case_pass_or_skip("pivot_root_namespace_root_rejected", case_namespace_root_rejected);
}

TEST(PivotRoot, NewRootOnRootMount) {
    expect_case_pass_or_skip("pivot_root_new_root_on_root_mount", case_new_root_on_root_mount);
}

TEST(PivotRoot, UnreachableNewRoot) {
    expect_case_pass_or_skip("pivot_root_unreachable_new_root", case_unreachable_new_root);
}

TEST(PivotRoot, NestedPivotPreservesUpperSibling) {
    expect_case_pass_or_skip("pivot_root_nested_preserves_upper_sibling",
                             case_nested_pivot_preserves_upper_sibling);
}

TEST(PivotRoot, PrivateStackedPutOld) {
    expect_case_pass_or_skip("pivot_root_private_stacked_put_old", case_private_stacked_put_old);
}

TEST(PivotRoot, CrossProcessExactFsRefs) {
    expect_case_pass_or_skip("pivot_root_cross_process_exact_fs_refs",
                             case_cross_process_exact_fs_refs);
}

TEST(PivotRoot, SymlinkToMountedNewRoot) {
    expect_case_pass_or_skip("pivot_root_symlink_to_mounted_new_root",
                             case_symlink_to_mounted_new_root);
}

TEST(PivotRoot, SelfBindAliasNewRoot) {
    expect_case_pass_or_skip("pivot_root_self_bind_alias_new_root",
                             case_self_bind_alias_new_root);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
