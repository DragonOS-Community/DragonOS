/**
 * test_dup_shared_fd.c - 验证 dup/dup2/dup3 共享 open file description 语义
 *
 * POSIX 规定 dup 创建的新 fd 与旧 fd 共享同一个 open file description，
 * 意味着它们共享文件偏移量和文件状态标志，但 close_on_exec 是 per-fd 独立的。
 *
 * 本测试覆盖：
 *   1. dup'd fd 共享文件偏移量（lseek 联动）
 *   2. dup'd fd 共享文件状态标志（O_APPEND）
 *   3. dup3(O_CLOEXEC) 只影响新 fd 的 close_on_exec
 *   4. dup2 覆盖已有 fd 后共享 offset
 *   5. 独立 open 的 fd 不共享 offset
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

static int test_count = 0;
static int fail_count = 0;

#define TEST_ASSERT(cond, msg)                                                 \
    do {                                                                       \
        test_count++;                                                          \
        if (!(cond)) {                                                         \
            fprintf(stderr, "FAIL [%d]: %s (line %d)\n", test_count, msg,      \
                    __LINE__);                                                 \
            fail_count++;                                                      \
        } else {                                                               \
            printf("PASS [%d]: %s\n", test_count, msg);                        \
        }                                                                      \
    } while (0)

#define TEST_FILE "/tmp/test_dup_shared_fd.tmp"

static void cleanup(void) { unlink(TEST_FILE); }

/**
 * 测试 1: dup'd fd 共享文件偏移量
 *
 * 对应 gvisor LseekTest::EtcPasswdDup：
 *   fd1 = open(file)
 *   fd2 = dup(fd1)
 *   lseek(fd1, 1000, SEEK_SET)
 *   assert(lseek(fd2, 0, SEEK_CUR) == 1000)  // fd2 看到 fd1 的偏移
 */
static void test_dup_shared_offset(void) {
    printf("\n--- test_dup_shared_offset ---\n");

    int fd1 = open(TEST_FILE, O_RDWR | O_CREAT | O_TRUNC, 0644);
    TEST_ASSERT(fd1 >= 0, "open test file");

    /* 写入一些数据 */
    char buf[2048];
    memset(buf, 'A', sizeof(buf));
    ssize_t nw = write(fd1, buf, sizeof(buf));
    TEST_ASSERT(nw == sizeof(buf), "write 2048 bytes");

    /* 回到开头 */
    off_t pos = lseek(fd1, 0, SEEK_SET);
    TEST_ASSERT(pos == 0, "lseek fd1 to 0");

    /* dup */
    int fd2 = dup(fd1);
    TEST_ASSERT(fd2 >= 0, "dup(fd1)");

    /* 两个 fd 都在 offset 0 */
    pos = lseek(fd1, 0, SEEK_CUR);
    TEST_ASSERT(pos == 0, "fd1 at offset 0");
    pos = lseek(fd2, 0, SEEK_CUR);
    TEST_ASSERT(pos == 0, "fd2 at offset 0 (shared)");

    /* 通过 fd1 seek 到 1000 */
    pos = lseek(fd1, 1000, SEEK_SET);
    TEST_ASSERT(pos == 1000, "lseek fd1 to 1000");

    /* fd2 应该也在 1000 — 这是 dup 共享 offset 的核心语义 */
    pos = lseek(fd2, 0, SEEK_CUR);
    TEST_ASSERT(pos == 1000, "fd2 also at 1000 after fd1 seek (shared offset)");

    /* 再 dup 一个 fd3，也应该在 1000 */
    int fd3 = dup(fd1);
    TEST_ASSERT(fd3 >= 0, "dup(fd1) -> fd3");
    pos = lseek(fd3, 0, SEEK_CUR);
    TEST_ASSERT(pos == 1000, "fd3 also at 1000 (shared offset)");

    close(fd3);
    close(fd2);
    close(fd1);
}

/**
 * 测试 2: 独立 open 的 fd 不共享 offset
 */
static void test_independent_open_no_share(void) {
    printf("\n--- test_independent_open_no_share ---\n");

    int fd1 = open(TEST_FILE, O_RDWR | O_CREAT | O_TRUNC, 0644);
    TEST_ASSERT(fd1 >= 0, "open fd1");
    char buf[1024];
    memset(buf, 'B', sizeof(buf));
    (void)write(fd1, buf, sizeof(buf));

    int fd2 = open(TEST_FILE, O_RDONLY);
    TEST_ASSERT(fd2 >= 0, "open fd2 independently");

    lseek(fd1, 500, SEEK_SET);
    off_t pos = lseek(fd2, 0, SEEK_CUR);
    TEST_ASSERT(pos == 0,
                "fd2 at 0, not affected by fd1 seek (independent open)");

    close(fd2);
    close(fd1);
}

/**
 * 测试 3: dup'd fd 共享文件状态标志 (O_APPEND)
 *
 * 通过 fcntl(F_SETFL) 在一个 fd 上设置 O_APPEND，
 * dup'd fd 也应该能看到 O_APPEND（因为它们共享同一个 open file description）。
 */
static void test_dup_shared_flags(void) {
    printf("\n--- test_dup_shared_flags ---\n");

    int fd1 = open(TEST_FILE, O_RDWR | O_CREAT | O_TRUNC, 0644);
    TEST_ASSERT(fd1 >= 0, "open test file");

    int fd2 = dup(fd1);
    TEST_ASSERT(fd2 >= 0, "dup(fd1)");

    /* 确认初始没有 O_APPEND */
    int flags1 = fcntl(fd1, F_GETFL);
    TEST_ASSERT(!(flags1 & O_APPEND), "fd1 initially no O_APPEND");
    int flags2 = fcntl(fd2, F_GETFL);
    TEST_ASSERT(!(flags2 & O_APPEND), "fd2 initially no O_APPEND");

    /* 通过 fd1 设置 O_APPEND */
    int ret = fcntl(fd1, F_SETFL, flags1 | O_APPEND);
    TEST_ASSERT(ret == 0, "fcntl F_SETFL O_APPEND on fd1");

    /* fd2 也应该看到 O_APPEND — 因为共享同一个 File */
    flags2 = fcntl(fd2, F_GETFL);
    TEST_ASSERT(flags2 & O_APPEND,
                "fd2 sees O_APPEND after fd1 set it (shared flags)");

    close(fd2);
    close(fd1);
}

/**
 * 测试 4: close_on_exec 是 per-fd 独立的
 *
 * dup() 默认不设置 cloexec。
 * dup3(fd, newfd, O_CLOEXEC) 只对 newfd 设置 cloexec，不影响 oldfd。
 * fcntl(F_SETFD, FD_CLOEXEC) 也只影响指定的 fd。
 */
static void test_cloexec_per_fd(void) {
    printf("\n--- test_cloexec_per_fd ---\n");

    int fd1 = open(TEST_FILE, O_RDWR | O_CREAT | O_TRUNC, 0644);
    TEST_ASSERT(fd1 >= 0, "open test file");

    /* dup() 默认不设置 cloexec */
    int fd2 = dup(fd1);
    TEST_ASSERT(fd2 >= 0, "dup(fd1)");

    int cloexec1 = fcntl(fd1, F_GETFD);
    int cloexec2 = fcntl(fd2, F_GETFD);
    TEST_ASSERT(!(cloexec1 & FD_CLOEXEC), "fd1 no cloexec");
    TEST_ASSERT(!(cloexec2 & FD_CLOEXEC), "fd2 no cloexec (dup default)");

    /* 通过 fcntl 在 fd1 设置 cloexec */
    fcntl(fd1, F_SETFD, FD_CLOEXEC);
    cloexec1 = fcntl(fd1, F_GETFD);
    cloexec2 = fcntl(fd2, F_GETFD);
    TEST_ASSERT(cloexec1 & FD_CLOEXEC, "fd1 has cloexec after F_SETFD");
    TEST_ASSERT(!(cloexec2 & FD_CLOEXEC),
                "fd2 still no cloexec (per-fd independent)");

    close(fd2);

    /* dup3 with O_CLOEXEC */
    int fd3 = 100; /* 使用一个高 fd 号 */
    int ret = dup3(fd1, fd3, O_CLOEXEC);
    TEST_ASSERT(ret == fd3, "dup3(fd1, 100, O_CLOEXEC)");

    int cloexec3 = fcntl(fd3, F_GETFD);
    cloexec1 = fcntl(fd1, F_GETFD);
    TEST_ASSERT(cloexec3 & FD_CLOEXEC, "fd3 has cloexec (dup3 O_CLOEXEC)");
    TEST_ASSERT(cloexec1 & FD_CLOEXEC,
                "fd1 cloexec unchanged (was set earlier)");

    /* 清除 fd1 的 cloexec，不影响 fd3 */
    fcntl(fd1, F_SETFD, 0);
    cloexec1 = fcntl(fd1, F_GETFD);
    cloexec3 = fcntl(fd3, F_GETFD);
    TEST_ASSERT(!(cloexec1 & FD_CLOEXEC), "fd1 cloexec cleared");
    TEST_ASSERT(cloexec3 & FD_CLOEXEC,
                "fd3 cloexec unchanged (per-fd independent)");

    close(fd3);
    close(fd1);
}

/**
 * 测试 5: dup2 覆盖已有 fd 后共享 offset
 */
static void test_dup2_shared_offset(void) {
    printf("\n--- test_dup2_shared_offset ---\n");

    int fd1 = open(TEST_FILE, O_RDWR | O_CREAT | O_TRUNC, 0644);
    TEST_ASSERT(fd1 >= 0, "open test file");
    char buf[2048];
    memset(buf, 'C', sizeof(buf));
    (void)write(fd1, buf, sizeof(buf));
    lseek(fd1, 500, SEEK_SET);

    /* 打开另一个 fd 用于被 dup2 覆盖 */
    int fd2 = open(TEST_FILE, O_RDONLY);
    TEST_ASSERT(fd2 >= 0, "open fd2");

    /* fd2 独立打开，offset 为 0 */
    off_t pos = lseek(fd2, 0, SEEK_CUR);
    TEST_ASSERT(pos == 0, "fd2 at 0 before dup2");

    /* dup2(fd1, fd2)：关闭旧 fd2，让 fd2 共享 fd1 的 File */
    int ret = dup2(fd1, fd2);
    TEST_ASSERT(ret == fd2, "dup2(fd1, fd2) returns fd2");

    /* 现在 fd2 应该和 fd1 共享 offset (500) */
    pos = lseek(fd2, 0, SEEK_CUR);
    TEST_ASSERT(pos == 500, "fd2 at 500 after dup2 (shared with fd1)");

    /* 通过 fd2 seek，fd1 也应该同步 */
    lseek(fd2, 1000, SEEK_SET);
    pos = lseek(fd1, 0, SEEK_CUR);
    TEST_ASSERT(pos == 1000, "fd1 at 1000 after fd2 seek (shared offset)");

    close(fd2);
    close(fd1);
}

/**
 * 测试 6: read 通过 dup'd fd 共享偏移量
 */
static void test_dup_read_advances_shared_offset(void) {
    printf("\n--- test_dup_read_advances_shared_offset ---\n");

    int fd1 = open(TEST_FILE, O_RDWR | O_CREAT | O_TRUNC, 0644);
    TEST_ASSERT(fd1 >= 0, "open test file");

    /* 写入 100 字节 */
    char wbuf[100];
    memset(wbuf, 'D', sizeof(wbuf));
    (void)write(fd1, wbuf, sizeof(wbuf));
    lseek(fd1, 0, SEEK_SET);

    int fd2 = dup(fd1);
    TEST_ASSERT(fd2 >= 0, "dup(fd1)");

    /* 通过 fd1 读 30 字节 */
    char rbuf[30];
    ssize_t nr = read(fd1, rbuf, sizeof(rbuf));
    TEST_ASSERT(nr == 30, "read 30 bytes via fd1");

    /* fd2 的偏移量应该也是 30 */
    off_t pos = lseek(fd2, 0, SEEK_CUR);
    TEST_ASSERT(pos == 30, "fd2 at 30 after fd1 read (shared offset)");

    /* 通过 fd2 再读 20 字节 */
    char rbuf2[20];
    nr = read(fd2, rbuf2, sizeof(rbuf2));
    TEST_ASSERT(nr == 20, "read 20 bytes via fd2");

    /* fd1 的偏移量应该是 50 */
    pos = lseek(fd1, 0, SEEK_CUR);
    TEST_ASSERT(pos == 50, "fd1 at 50 after fd2 read (shared offset)");

    close(fd2);
    close(fd1);
}

/**
 * 测试 7: dup2(oldfd, oldfd) 返回 oldfd 且不做任何改变
 */
static void test_dup2_same_fd(void) {
    printf("\n--- test_dup2_same_fd ---\n");

    int fd = open(TEST_FILE, O_RDWR | O_CREAT | O_TRUNC, 0644);
    TEST_ASSERT(fd >= 0, "open test file");

    int ret = dup2(fd, fd);
    TEST_ASSERT(ret == fd, "dup2(fd, fd) returns fd");

    /* fd 仍然有效 */
    int flags = fcntl(fd, F_GETFL);
    TEST_ASSERT(flags >= 0, "fd still valid after dup2(fd, fd)");

    close(fd);
}

/**
 * 测试 8: dup2 到高位 fd（超出初始 fd 表大小）
 *
 * Linux 的 ksys_dup3 调用 expand_files(files, newfd) 自动扩容 fd 表，
 * 只要 newfd < RLIMIT_NOFILE 就是合法的。
 * 验证 BUG-3 修复：之前 validate_fd(newfd) 阻止了高位 fd。
 */
static void test_dup2_high_fd(void) {
    printf("\n--- test_dup2_high_fd ---\n");

    int fd1 = open(TEST_FILE, O_RDWR | O_CREAT | O_TRUNC, 0644);
    TEST_ASSERT(fd1 >= 0, "open test file");

    /* 写入一些数据 */
    char buf[64];
    memset(buf, 'E', sizeof(buf));
    (void)write(fd1, buf, sizeof(buf));
    lseek(fd1, 42, SEEK_SET);

    /* dup2 到一个高位 fd（超出默认 fd 表大小 1024） */
    int high_fd = 1500;
    int ret = dup2(fd1, high_fd);
    TEST_ASSERT(ret == high_fd, "dup2(fd1, 1500) succeeds");

    /* 验证高位 fd 和原 fd 共享 offset */
    off_t pos = lseek(high_fd, 0, SEEK_CUR);
    TEST_ASSERT(pos == 42, "high_fd at 42 (shared offset with fd1)");

    /* 通过高位 fd seek，原 fd 也应该同步 */
    lseek(high_fd, 99, SEEK_SET);
    pos = lseek(fd1, 0, SEEK_CUR);
    TEST_ASSERT(pos == 99, "fd1 at 99 after high_fd seek (shared offset)");

    /* 验证高位 fd 的 cloexec 默认为 false（dup2 语义） */
    int cloexec = fcntl(high_fd, F_GETFD);
    TEST_ASSERT(!(cloexec & FD_CLOEXEC), "high_fd no cloexec (dup2 default)");

    close(high_fd);
    close(fd1);
}

/**
 * 测试 9: dup3(fd, fd, 0) 返回 EINVAL
 *
 * POSIX/Linux 规定 dup3 在 oldfd == newfd 时必须返回 EINVAL，
 * 与 dup2(fd, fd) 的 no-op 语义不同。
 */
static void test_dup3_same_fd_einval(void) {
    printf("\n--- test_dup3_same_fd_einval ---\n");

    int fd = open(TEST_FILE, O_RDWR | O_CREAT | O_TRUNC, 0644);
    TEST_ASSERT(fd >= 0, "open test file");

    int ret = dup3(fd, fd, 0);
    TEST_ASSERT(ret == -1, "dup3(fd, fd, 0) returns -1");
    TEST_ASSERT(errno == EINVAL, "dup3(fd, fd, 0) sets errno to EINVAL");

    /* 同样带 O_CLOEXEC 的情况 */
    ret = dup3(fd, fd, O_CLOEXEC);
    TEST_ASSERT(ret == -1, "dup3(fd, fd, O_CLOEXEC) returns -1");
    TEST_ASSERT(errno == EINVAL,
                "dup3(fd, fd, O_CLOEXEC) sets errno to EINVAL");

    /* fd 仍然有效 */
    int flags = fcntl(fd, F_GETFL);
    TEST_ASSERT(flags >= 0, "fd still valid after failed dup3");

    close(fd);
}

/**
 * 测试 10: 关闭原 fd 后 dup'd fd 仍然有效（引用计数正确）
 *
 * dup 共享 Arc<File>，关闭原 fd 只是减少引用计数，
 * dup'd fd 仍然持有引用，应该可以正常使用。
 */
static void test_dup_close_original(void) {
    printf("\n--- test_dup_close_original ---\n");

    int fd1 = open(TEST_FILE, O_RDWR | O_CREAT | O_TRUNC, 0644);
    TEST_ASSERT(fd1 >= 0, "open test file");

    /* 写入数据 */
    const char *msg = "Hello, dup refcount!";
    ssize_t nw = write(fd1, msg, strlen(msg));
    TEST_ASSERT(nw == (ssize_t)strlen(msg), "write message");

    /* dup fd1 -> fd2 */
    int fd2 = dup(fd1);
    TEST_ASSERT(fd2 >= 0, "dup(fd1)");

    /* 关闭原 fd */
    close(fd1);

    /* fd2 仍然有效：可以 seek 和 read */
    off_t pos = lseek(fd2, 0, SEEK_SET);
    TEST_ASSERT(pos == 0, "lseek fd2 to 0 after closing fd1");

    char rbuf[64];
    memset(rbuf, 0, sizeof(rbuf));
    ssize_t nr = read(fd2, rbuf, sizeof(rbuf));
    TEST_ASSERT(nr == (ssize_t)strlen(msg), "read from fd2 after closing fd1");
    TEST_ASSERT(memcmp(rbuf, msg, strlen(msg)) == 0,
                "fd2 reads correct data after fd1 closed");

    /* 通过 fd2 写入更多数据 */
    const char *msg2 = " Still works!";
    nw = write(fd2, msg2, strlen(msg2));
    TEST_ASSERT(nw == (ssize_t)strlen(msg2),
                "write via fd2 after fd1 closed");

    close(fd2);
}

int main(void) {
    cleanup();

    test_dup_shared_offset();
    test_independent_open_no_share();
    test_dup_shared_flags();
    test_cloexec_per_fd();
    test_dup2_shared_offset();
    test_dup_read_advances_shared_offset();
    test_dup2_same_fd();
    test_dup2_high_fd();
    test_dup3_same_fd_einval();
    test_dup_close_original();

    cleanup();

    printf("\n========================================\n");
    printf("Total: %d tests, %d passed, %d failed\n", test_count,
           test_count - fail_count, fail_count);
    printf("========================================\n");

    return fail_count > 0 ? 1 : 0;
}
