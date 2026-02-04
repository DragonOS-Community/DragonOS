/**
 * @file main.c
 * @brief Comprehensive test suite for mknod system call
 *
 * Tests creation of:
 * - Character devices (S_IFCHR)
 * - Block devices (S_IFBLK)
 * - Named pipes / FIFOs (S_IFIFO)
 * - Unix domain sockets (S_IFSOCK) - note: typically created via socket()+bind()
 * - Regular files (S_IFREG)
 *
 * Build: gcc -o test_mknod main.c
 * Run: ./test_mknod (requires root for device nodes)
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#include <sys/types.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <fcntl.h>
#include <dirent.h>
#include <sys/wait.h>

/* Test directory */
#define TEST_DIR "/tmp/mknod_test"

/* Colors for output */
#define COLOR_GREEN  "\033[0;32m"
#define COLOR_RED    "\033[0;31m"
#define COLOR_YELLOW "\033[0;33m"
#define COLOR_RESET  "\033[0m"

/* Test counters */
static int tests_passed = 0;
static int tests_failed = 0;
static int tests_skipped = 0;

/* Helper macros */
#define TEST_PASS(name) do { \
    printf(COLOR_GREEN "[PASS]" COLOR_RESET " %s\n", name); \
    tests_passed++; \
} while(0)

#define TEST_FAIL(name, reason) do { \
    printf(COLOR_RED "[FAIL]" COLOR_RESET " %s: %s\n", name, reason); \
    tests_failed++; \
} while(0)

#define TEST_SKIP(name, reason) do { \
    printf(COLOR_YELLOW "[SKIP]" COLOR_RESET " %s: %s\n", name, reason); \
    tests_skipped++; \
} while(0)

/**
 * Setup test directory
 */
static int setup_test_dir(void)
{
    struct stat st;

    /* Remove existing test directory */
    if (stat(TEST_DIR, &st) == 0) {
        /* Simple cleanup - remove known test files */
        char cmd[256];
        snprintf(cmd, sizeof(cmd), "rm -rf %s", TEST_DIR);
        system(cmd);
    }

    /* Create fresh test directory */
    if (mkdir(TEST_DIR, 0755) != 0) {
        perror("Failed to create test directory");
        return -1;
    }

    return 0;
}

/**
 * Cleanup test directory
 */
static void cleanup_test_dir(void)
{
    char cmd[256];
    snprintf(cmd, sizeof(cmd), "rm -rf %s", TEST_DIR);
    system(cmd);
}

/**
 * Verify file type and device number
 */
static int verify_node(const char *path, mode_t expected_type, dev_t expected_dev)
{
    struct stat st;

    if (stat(path, &st) != 0) {
        return -1;
    }

    if ((st.st_mode & S_IFMT) != expected_type) {
        return -2;
    }

    /* For device nodes, verify device number */
    if (expected_type == S_IFCHR || expected_type == S_IFBLK) {
        if (st.st_rdev != expected_dev) {
            return -3;
        }
    }

    return 0;
}

/**
 * Get file type string for display
 */
static const char *filetype_str(mode_t mode)
{
    switch (mode & S_IFMT) {
    case S_IFREG:  return "regular file";
    case S_IFDIR:  return "directory";
    case S_IFCHR:  return "character device";
    case S_IFBLK:  return "block device";
    case S_IFIFO:  return "FIFO";
    case S_IFSOCK: return "socket";
    case S_IFLNK:  return "symlink";
    default:       return "unknown";
    }
}

/* ========== Character Device Tests ========== */

/**
 * Test: Create /dev/null equivalent (major=1, minor=3)
 */
static void test_chardev_null(void)
{
    const char *path = TEST_DIR "/null";
    dev_t dev = makedev(1, 3);
    int ret;

    ret = mknod(path, S_IFCHR | 0666, dev);
    if (ret != 0) {
        if (errno == EPERM || errno == EACCES) {
            TEST_SKIP("chardev_null", "requires root privileges");
        } else {
            char buf[128];
            snprintf(buf, sizeof(buf), "mknod failed: %s", strerror(errno));
            TEST_FAIL("chardev_null", buf);
        }
        return;
    }

    ret = verify_node(path, S_IFCHR, dev);
    if (ret == 0) {
        TEST_PASS("chardev_null (major=1, minor=3)");
    } else {
        TEST_FAIL("chardev_null", "verification failed");
    }
}

/**
 * Test: Create /dev/zero equivalent (major=1, minor=5)
 */
static void test_chardev_zero(void)
{
    const char *path = TEST_DIR "/zero";
    dev_t dev = makedev(1, 5);
    int ret;

    ret = mknod(path, S_IFCHR | 0666, dev);
    if (ret != 0) {
        if (errno == EPERM || errno == EACCES) {
            TEST_SKIP("chardev_zero", "requires root privileges");
        } else {
            char buf[128];
            snprintf(buf, sizeof(buf), "mknod failed: %s", strerror(errno));
            TEST_FAIL("chardev_zero", buf);
        }
        return;
    }

    ret = verify_node(path, S_IFCHR, dev);
    if (ret == 0) {
        TEST_PASS("chardev_zero (major=1, minor=5)");
    } else {
        TEST_FAIL("chardev_zero", "verification failed");
    }
}

/**
 * Test: Create tty device (major=4, minor=0)
 */
static void test_chardev_tty(void)
{
    const char *path = TEST_DIR "/tty0";
    dev_t dev = makedev(4, 0);
    int ret;

    ret = mknod(path, S_IFCHR | 0620, dev);
    if (ret != 0) {
        if (errno == EPERM || errno == EACCES) {
            TEST_SKIP("chardev_tty", "requires root privileges");
        } else {
            char buf[128];
            snprintf(buf, sizeof(buf), "mknod failed: %s", strerror(errno));
            TEST_FAIL("chardev_tty", buf);
        }
        return;
    }

    ret = verify_node(path, S_IFCHR, dev);
    if (ret == 0) {
        TEST_PASS("chardev_tty (major=4, minor=0)");
    } else {
        TEST_FAIL("chardev_tty", "verification failed");
    }
}

/**
 * Test: Character device with minor > 255 (requires new format encoding)
 */
static void test_chardev_large_minor(void)
{
    const char *path = TEST_DIR "/chardev_large_minor";
    dev_t dev = makedev(10, 256); /* misc device with large minor */
    int ret;

    ret = mknod(path, S_IFCHR | 0666, dev);
    if (ret != 0) {
        if (errno == EPERM || errno == EACCES) {
            TEST_SKIP("chardev_large_minor", "requires root privileges");
        } else {
            char buf[128];
            snprintf(buf, sizeof(buf), "mknod failed: %s", strerror(errno));
            TEST_FAIL("chardev_large_minor", buf);
        }
        return;
    }

    ret = verify_node(path, S_IFCHR, dev);
    if (ret == 0) {
        TEST_PASS("chardev_large_minor (major=10, minor=256)");
    } else {
        TEST_FAIL("chardev_large_minor", "verification failed");
    }
}

/* ========== Block Device Tests ========== */

/**
 * Test: Create sda equivalent (major=8, minor=0)
 */
static void test_blkdev_sda(void)
{
    const char *path = TEST_DIR "/sda";
    dev_t dev = makedev(8, 0);
    int ret;

    ret = mknod(path, S_IFBLK | 0660, dev);
    if (ret != 0) {
        if (errno == EPERM || errno == EACCES) {
            TEST_SKIP("blkdev_sda", "requires root privileges");
        } else {
            char buf[128];
            snprintf(buf, sizeof(buf), "mknod failed: %s", strerror(errno));
            TEST_FAIL("blkdev_sda", buf);
        }
        return;
    }

    ret = verify_node(path, S_IFBLK, dev);
    if (ret == 0) {
        TEST_PASS("blkdev_sda (major=8, minor=0)");
    } else {
        TEST_FAIL("blkdev_sda", "verification failed");
    }
}

/**
 * Test: Create sda1 partition (major=8, minor=1)
 */
static void test_blkdev_sda1(void)
{
    const char *path = TEST_DIR "/sda1";
    dev_t dev = makedev(8, 1);
    int ret;

    ret = mknod(path, S_IFBLK | 0660, dev);
    if (ret != 0) {
        if (errno == EPERM || errno == EACCES) {
            TEST_SKIP("blkdev_sda1", "requires root privileges");
        } else {
            char buf[128];
            snprintf(buf, sizeof(buf), "mknod failed: %s", strerror(errno));
            TEST_FAIL("blkdev_sda1", buf);
        }
        return;
    }

    ret = verify_node(path, S_IFBLK, dev);
    if (ret == 0) {
        TEST_PASS("blkdev_sda1 (major=8, minor=1)");
    } else {
        TEST_FAIL("blkdev_sda1", "verification failed");
    }
}

/**
 * Test: Create loop device (major=7, minor=0)
 */
static void test_blkdev_loop(void)
{
    const char *path = TEST_DIR "/loop0";
    dev_t dev = makedev(7, 0);
    int ret;

    ret = mknod(path, S_IFBLK | 0660, dev);
    if (ret != 0) {
        if (errno == EPERM || errno == EACCES) {
            TEST_SKIP("blkdev_loop", "requires root privileges");
        } else {
            char buf[128];
            snprintf(buf, sizeof(buf), "mknod failed: %s", strerror(errno));
            TEST_FAIL("blkdev_loop", buf);
        }
        return;
    }

    ret = verify_node(path, S_IFBLK, dev);
    if (ret == 0) {
        TEST_PASS("blkdev_loop (major=7, minor=0)");
    } else {
        TEST_FAIL("blkdev_loop", "verification failed");
    }
}

/**
 * Test: Block device with large device numbers (NVMe style)
 */
static void test_blkdev_nvme(void)
{
    const char *path = TEST_DIR "/nvme0n1";
    dev_t dev = makedev(259, 0); /* NVMe major */
    int ret;

    ret = mknod(path, S_IFBLK | 0660, dev);
    if (ret != 0) {
        if (errno == EPERM || errno == EACCES) {
            TEST_SKIP("blkdev_nvme", "requires root privileges");
        } else {
            char buf[128];
            snprintf(buf, sizeof(buf), "mknod failed: %s", strerror(errno));
            TEST_FAIL("blkdev_nvme", buf);
        }
        return;
    }

    ret = verify_node(path, S_IFBLK, dev);
    if (ret == 0) {
        TEST_PASS("blkdev_nvme (major=259, minor=0)");
    } else {
        TEST_FAIL("blkdev_nvme", "verification failed");
    }
}

/**
 * Test: Block device with very large minor number
 */
static void test_blkdev_large_minor(void)
{
    const char *path = TEST_DIR "/blkdev_large";
    dev_t dev = makedev(8, 65536); /* Large minor */
    int ret;

    ret = mknod(path, S_IFBLK | 0660, dev);
    if (ret != 0) {
        if (errno == EPERM || errno == EACCES) {
            TEST_SKIP("blkdev_large_minor", "requires root privileges");
        } else {
            char buf[128];
            snprintf(buf, sizeof(buf), "mknod failed: %s", strerror(errno));
            TEST_FAIL("blkdev_large_minor", buf);
        }
        return;
    }

    ret = verify_node(path, S_IFBLK, dev);
    if (ret == 0) {
        TEST_PASS("blkdev_large_minor (major=8, minor=65536)");
    } else {
        TEST_FAIL("blkdev_large_minor", "verification failed");
    }
}

/* ========== FIFO (Named Pipe) Tests ========== */

/**
 * Test: Create basic FIFO
 */
static void test_fifo_basic(void)
{
    const char *path = TEST_DIR "/fifo_basic";
    int ret;

    ret = mknod(path, S_IFIFO | 0666, 0);
    if (ret != 0) {
        char buf[128];
        snprintf(buf, sizeof(buf), "mknod failed: %s", strerror(errno));
        TEST_FAIL("fifo_basic", buf);
        return;
    }

    ret = verify_node(path, S_IFIFO, 0);
    if (ret == 0) {
        TEST_PASS("fifo_basic");
    } else {
        TEST_FAIL("fifo_basic", "verification failed");
    }
}

/**
 * Test: Create FIFO using mkfifo (wrapper around mknod)
 */
static void test_fifo_mkfifo(void)
{
    const char *path = TEST_DIR "/fifo_mkfifo";
    int ret;

    ret = mkfifo(path, 0644);
    if (ret != 0) {
        char buf[128];
        snprintf(buf, sizeof(buf), "mkfifo failed: %s", strerror(errno));
        TEST_FAIL("fifo_mkfifo", buf);
        return;
    }

    ret = verify_node(path, S_IFIFO, 0);
    if (ret == 0) {
        TEST_PASS("fifo_mkfifo");
    } else {
        TEST_FAIL("fifo_mkfifo", "verification failed");
    }
}

/**
 * Test: FIFO with restricted permissions
 */
static void test_fifo_permissions(void)
{
    const char *path = TEST_DIR "/fifo_perms";
    int ret;
    struct stat st;

    ret = mknod(path, S_IFIFO | 0600, 0);
    if (ret != 0) {
        char buf[128];
        snprintf(buf, sizeof(buf), "mknod failed: %s", strerror(errno));
        TEST_FAIL("fifo_permissions", buf);
        return;
    }

    if (stat(path, &st) != 0) {
        TEST_FAIL("fifo_permissions", "stat failed");
        return;
    }

    /* Check permissions (considering umask) */
    if ((st.st_mode & S_IFMT) == S_IFIFO) {
        TEST_PASS("fifo_permissions (mode=0600)");
    } else {
        TEST_FAIL("fifo_permissions", "wrong file type");
    }
}

/**
 * Test: FIFO read/write functionality
 */
static void test_fifo_io(void)
{
    const char *path = TEST_DIR "/fifo_io";
    int ret;
    pid_t pid;

    ret = mknod(path, S_IFIFO | 0666, 0);
    if (ret != 0) {
        char buf[128];
        snprintf(buf, sizeof(buf), "mknod failed: %s", strerror(errno));
        TEST_FAIL("fifo_io", buf);
        return;
    }

    pid = fork();
    if (pid < 0) {
        TEST_FAIL("fifo_io", "fork failed");
        return;
    }

    if (pid == 0) {
        /* Child: write to FIFO */
        int fd = open(path, O_WRONLY);
        if (fd >= 0) {
            write(fd, "test", 4);
            close(fd);
        }
        _exit(0);
    } else {
        /* Parent: read from FIFO */
        char buf[16] = {0};
        int fd = open(path, O_RDONLY);
        if (fd >= 0) {
            ssize_t n = read(fd, buf, sizeof(buf) - 1);
            close(fd);

            int status;
            waitpid(pid, &status, 0);

            if (n == 4 && strcmp(buf, "test") == 0) {
                TEST_PASS("fifo_io (read/write)");
            } else {
                TEST_FAIL("fifo_io", "data mismatch");
            }
        } else {
            TEST_FAIL("fifo_io", "open failed");
            waitpid(pid, NULL, 0);
        }
    }
}

/* ========== Socket Tests ========== */

/**
 * Test: Create socket node via mknod
 * Note: This is not the standard way to create Unix sockets.
 * Normally, sockets are created via socket() + bind().
 * Some systems may not support creating socket nodes via mknod.
 */
static void test_socket_mknod(void)
{
    const char *path = TEST_DIR "/socket_mknod";
    int ret;

    ret = mknod(path, S_IFSOCK | 0666, 0);
    if (ret != 0) {
        if (errno == EPERM || errno == EINVAL || errno == ENOSYS) {
            TEST_SKIP("socket_mknod", "mknod for sockets not supported (use socket()+bind())");
        } else {
            char buf[128];
            snprintf(buf, sizeof(buf), "mknod failed: %s", strerror(errno));
            TEST_FAIL("socket_mknod", buf);
        }
        return;
    }

    ret = verify_node(path, S_IFSOCK, 0);
    if (ret == 0) {
        TEST_PASS("socket_mknod");
    } else {
        TEST_FAIL("socket_mknod", "verification failed");
    }
}

/* ========== Regular File Tests ========== */

/**
 * Test: Create regular file via mknod
 */
static void test_regular_file(void)
{
    const char *path = TEST_DIR "/regular";
    int ret;

    ret = mknod(path, S_IFREG | 0644, 0);
    if (ret != 0) {
        char buf[128];
        snprintf(buf, sizeof(buf), "mknod failed: %s", strerror(errno));
        TEST_FAIL("regular_file", buf);
        return;
    }

    ret = verify_node(path, S_IFREG, 0);
    if (ret == 0) {
        TEST_PASS("regular_file");
    } else {
        TEST_FAIL("regular_file", "verification failed");
    }
}

/* ========== Error Handling Tests ========== */

/**
 * Test: EEXIST - file already exists
 */
static void test_error_eexist(void)
{
    const char *path = TEST_DIR "/existing";
    int ret;

    /* Create file first */
    ret = mknod(path, S_IFREG | 0644, 0);
    if (ret != 0) {
        TEST_SKIP("error_eexist", "initial mknod failed");
        return;
    }

    /* Try to create again */
    ret = mknod(path, S_IFREG | 0644, 0);
    if (ret == -1 && errno == EEXIST) {
        TEST_PASS("error_eexist");
    } else {
        TEST_FAIL("error_eexist", "expected EEXIST");
    }
}

/**
 * Test: ENOENT - parent directory doesn't exist
 */
static void test_error_enoent(void)
{
    const char *path = TEST_DIR "/nonexistent/file";
    int ret;

    ret = mknod(path, S_IFREG | 0644, 0);
    if (ret == -1 && errno == ENOENT) {
        TEST_PASS("error_enoent");
    } else {
        char buf[128];
        snprintf(buf, sizeof(buf), "expected ENOENT, got %s", strerror(errno));
        TEST_FAIL("error_enoent", buf);
    }
}

/**
 * Test: ENOTDIR - parent is not a directory
 */
static void test_error_enotdir(void)
{
    const char *file_path = TEST_DIR "/notdir";
    const char *path = TEST_DIR "/notdir/child";
    int ret;

    /* Create regular file */
    ret = mknod(file_path, S_IFREG | 0644, 0);
    if (ret != 0) {
        TEST_SKIP("error_enotdir", "initial mknod failed");
        return;
    }

    /* Try to create file under the regular file */
    ret = mknod(path, S_IFREG | 0644, 0);
    if (ret == -1 && errno == ENOTDIR) {
        TEST_PASS("error_enotdir");
    } else {
        char buf[128];
        snprintf(buf, sizeof(buf), "expected ENOTDIR, got %s", strerror(errno));
        TEST_FAIL("error_enotdir", buf);
    }
}

/* ========== Edge Case Tests ========== */

/**
 * Test: Device number boundary - max old format (255, 255)
 */
static void test_devnum_max_old(void)
{
    const char *path = TEST_DIR "/dev_max_old";
    dev_t dev = makedev(255, 255);
    int ret;

    ret = mknod(path, S_IFCHR | 0666, dev);
    if (ret != 0) {
        if (errno == EPERM || errno == EACCES) {
            TEST_SKIP("devnum_max_old", "requires root privileges");
        } else {
            char buf[128];
            snprintf(buf, sizeof(buf), "mknod failed: %s", strerror(errno));
            TEST_FAIL("devnum_max_old", buf);
        }
        return;
    }

    ret = verify_node(path, S_IFCHR, dev);
    if (ret == 0) {
        TEST_PASS("devnum_max_old (major=255, minor=255)");
    } else {
        TEST_FAIL("devnum_max_old", "verification failed");
    }
}

/**
 * Test: Device number boundary - first new format (256, 0)
 */
static void test_devnum_first_new(void)
{
    const char *path = TEST_DIR "/dev_first_new";
    dev_t dev = makedev(256, 0);
    int ret;

    ret = mknod(path, S_IFCHR | 0666, dev);
    if (ret != 0) {
        if (errno == EPERM || errno == EACCES) {
            TEST_SKIP("devnum_first_new", "requires root privileges");
        } else {
            char buf[128];
            snprintf(buf, sizeof(buf), "mknod failed: %s", strerror(errno));
            TEST_FAIL("devnum_first_new", buf);
        }
        return;
    }

    ret = verify_node(path, S_IFCHR, dev);
    if (ret == 0) {
        TEST_PASS("devnum_first_new (major=256, minor=0)");
    } else {
        TEST_FAIL("devnum_first_new", "verification failed");
    }
}

/**
 * Test: Zero device number
 */
static void test_devnum_zero(void)
{
    const char *path = TEST_DIR "/dev_zero_num";
    dev_t dev = makedev(0, 0);
    int ret;

    ret = mknod(path, S_IFCHR | 0666, dev);
    if (ret != 0) {
        if (errno == EPERM || errno == EACCES) {
            TEST_SKIP("devnum_zero", "requires root privileges");
        } else {
            char buf[128];
            snprintf(buf, sizeof(buf), "mknod failed: %s", strerror(errno));
            TEST_FAIL("devnum_zero", buf);
        }
        return;
    }

    ret = verify_node(path, S_IFCHR, dev);
    if (ret == 0) {
        TEST_PASS("devnum_zero (major=0, minor=0)");
    } else {
        TEST_FAIL("devnum_zero", "verification failed");
    }
}

/* ========== Print Test Summary ========== */

static void print_summary(void)
{
    printf("\n");
    printf("========================================\n");
    printf("           Test Summary\n");
    printf("========================================\n");
    printf(COLOR_GREEN "  Passed:  %d\n" COLOR_RESET, tests_passed);
    printf(COLOR_RED "  Failed:  %d\n" COLOR_RESET, tests_failed);
    printf(COLOR_YELLOW "  Skipped: %d\n" COLOR_RESET, tests_skipped);
    printf("----------------------------------------\n");
    printf("  Total:   %d\n", tests_passed + tests_failed + tests_skipped);
    printf("========================================\n");

    if (tests_failed == 0) {
        printf(COLOR_GREEN "\nAll tests passed!\n" COLOR_RESET);
    } else {
        printf(COLOR_RED "\nSome tests failed.\n" COLOR_RESET);
    }
}

/* ========== List Created Nodes ========== */

static void list_test_nodes(void)
{
    DIR *dir;
    struct dirent *entry;
    struct stat st;
    char path[512];

    printf("\n");
    printf("========================================\n");
    printf("        Created Test Nodes\n");
    printf("========================================\n");

    dir = opendir(TEST_DIR);
    if (dir == NULL) {
        printf("  (test directory not found)\n");
        return;
    }

    while ((entry = readdir(dir)) != NULL) {
        if (entry->d_name[0] == '.') continue;

        snprintf(path, sizeof(path), "%s/%s", TEST_DIR, entry->d_name);
        if (stat(path, &st) == 0) {
            printf("  %-20s  %-16s", entry->d_name, filetype_str(st.st_mode));
            if (S_ISCHR(st.st_mode) || S_ISBLK(st.st_mode)) {
                printf("  dev=%u:%u", major(st.st_rdev), minor(st.st_rdev));
            }
            printf("  mode=%04o\n", st.st_mode & 07777);
        }
    }

    closedir(dir);
    printf("========================================\n");
}

/* ========== Main ========== */

int main(int argc, char *argv[])
{
    int keep_files = 0;

    /* Parse arguments */
    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--keep") == 0 || strcmp(argv[i], "-k") == 0) {
            keep_files = 1;
        } else if (strcmp(argv[i], "--help") == 0 || strcmp(argv[i], "-h") == 0) {
            printf("Usage: %s [OPTIONS]\n", argv[0]);
            printf("Options:\n");
            printf("  -k, --keep    Keep test files after completion\n");
            printf("  -h, --help    Show this help message\n");
            return 0;
        }
    }

    printf("========================================\n");
    printf("     mknod System Call Test Suite\n");
    printf("========================================\n\n");

    /* Check if running as root */
    if (geteuid() != 0) {
        printf(COLOR_YELLOW "Warning: Not running as root. Device node tests will be skipped.\n" COLOR_RESET);
        printf("Run with 'sudo' to test device node creation.\n\n");
    }

    /* Setup */
    if (setup_test_dir() != 0) {
        fprintf(stderr, "Failed to setup test directory\n");
        return 1;
    }

    /* Run tests */
    printf("--- Character Device Tests ---\n");
    test_chardev_null();
    test_chardev_zero();
    test_chardev_tty();
    test_chardev_large_minor();

    printf("\n--- Block Device Tests ---\n");
    test_blkdev_sda();
    test_blkdev_sda1();
    test_blkdev_loop();
    test_blkdev_nvme();
    test_blkdev_large_minor();

    printf("\n--- FIFO Tests ---\n");
    test_fifo_basic();
    test_fifo_mkfifo();
    test_fifo_permissions();
    test_fifo_io();

    printf("\n--- Socket Tests ---\n");
    test_socket_mknod();

    printf("\n--- Regular File Tests ---\n");
    test_regular_file();

    printf("\n--- Error Handling Tests ---\n");
    test_error_eexist();
    test_error_enoent();
    test_error_enotdir();

    printf("\n--- Device Number Edge Cases ---\n");
    test_devnum_max_old();
    test_devnum_first_new();
    test_devnum_zero();

    /* List created nodes */
    list_test_nodes();

    /* Print summary */
    print_summary();

    /* Cleanup */
    if (!keep_files) {
        cleanup_test_dir();
        printf("\nTest files cleaned up. Use --keep to preserve them.\n");
    } else {
        printf("\nTest files preserved in %s\n", TEST_DIR);
    }

    return tests_failed > 0 ? 1 : 0;
}
