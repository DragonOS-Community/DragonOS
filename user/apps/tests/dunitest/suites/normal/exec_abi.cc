#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

namespace {

#if defined(__x86_64__)

constexpr unsigned char kCheckRdxElf[] = {
    0x7f, 0x45, 0x4c, 0x46, 0x02, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x3e, 0x00, 0x01, 0x00, 0x00, 0x00,
    0x78, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x38, 0x00, 0x01, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00,

    0x01, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x92, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x92, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,

    // _start:
    //   test %rdx,%rdx
    //   jnz fail
    //   exit(0)
    // fail:
    //   exit(42)
    0x48, 0x85, 0xd2, 0x75, 0x09, 0x31, 0xff, 0xb8, 0x3c, 0x00, 0x00, 0x00,
    0x0f, 0x05, 0xbf, 0x2a, 0x00, 0x00, 0x00, 0xb8, 0x3c, 0x00, 0x00, 0x00,
    0x0f, 0x05,
};

void write_all(int fd, const void* data, size_t size) {
    const char* p = static_cast<const char*>(data);
    while (size > 0) {
        ssize_t n = write(fd, p, size);
        ASSERT_GT(n, 0) << "write failed: errno=" << errno << " (" << strerror(errno) << ")";
        p += n;
        size -= static_cast<size_t>(n);
    }
}

void write_check_rdx_elf(char* path, size_t path_size) {
    snprintf(path, path_size, "/tmp/exec_abi_check_rdx_%d", getpid());
    int fd = open(path, O_CREAT | O_TRUNC | O_WRONLY, 0755);
    ASSERT_GE(fd, 0) << "open(" << path << ") failed: errno=" << errno << " ("
                     << strerror(errno) << ")";

    write_all(fd, kCheckRdxElf, sizeof(kCheckRdxElf));
    ASSERT_EQ(0, close(fd)) << "close(" << path << ") failed: errno=" << errno << " ("
                            << strerror(errno) << ")";
    ASSERT_EQ(0, chmod(path, 0755)) << "chmod(" << path << ") failed: errno=" << errno << " ("
                                    << strerror(errno) << ")";
}

#endif

void ensure_tmp_dir() {
    if (mkdir("/tmp", 0755) != 0 && errno != EEXIST) {
        FAIL() << "mkdir(/tmp) failed: errno=" << errno << " (" << strerror(errno) << ")";
    }
}

}  // namespace

TEST(ExecAbi, X86_64ExecClearsRdxForProgramEntry) {
#if !defined(__x86_64__)
    GTEST_SKIP() << "x86_64-specific exec register ABI test";
#else
    ensure_tmp_dir();
#endif
}

#if defined(__x86_64__)

TEST(ExecAbi, X86_64ExecClearsRdxWhenEnvpIsNonNull) {
    ensure_tmp_dir();

    char path[128] = {};
    write_check_rdx_elf(path, sizeof(path));

    pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";

    if (child == 0) {
        char arg0[] = "check-rdx";
        char env0[] = "DRAGONOS_EXEC_ABI_RDX=non-null-envp";
        char* const argv[] = {arg0, nullptr};
        char* const envp[] = {env0, nullptr};
        execve(path, argv, envp);
        _exit(errno);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0))
        << "waitpid failed: errno=" << errno << " (" << strerror(errno) << ")";
    unlink(path);

    ASSERT_TRUE(WIFEXITED(status)) << "child did not exit normally, status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status))
        << "exec entry %rdx was not cleared; exit 42 means old envp leaked into %rdx";
}

#endif

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
