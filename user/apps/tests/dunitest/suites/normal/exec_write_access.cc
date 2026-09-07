#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#include <string>

namespace {
std::string self_path;

bool byte_io(int fd, bool writing) {
    char byte = 'R';
    ssize_t n;
    do {
        n = writing ? write(fd, &byte, 1) : read(fd, &byte, 1);
    } while (n < 0 && errno == EINTR);
    return n == 1;
}

class ExecWriteAccess : public ::testing::Test {
protected:
    std::string directory, image, alias;
    pid_t child = -1;
    int ready[2] = {-1, -1}, release[2] = {-1, -1};

    void SetUp() override {
        char dir[] = "/tmp/exec-write-access-XXXXXX";
        ASSERT_NE(nullptr, mkdtemp(dir));
        directory = dir;
        image = directory + "/image";
        alias = directory + "/alias";
        int in = open(self_path.c_str(), O_RDONLY);
        ASSERT_GE(in, 0);
        int out = open(image.c_str(), O_WRONLY | O_CREAT | O_EXCL, 0700);
        ASSERT_GE(out, 0);
        char buf[16384];
        ssize_t n;
        while ((n = read(in, buf, sizeof(buf))) > 0) {
            ssize_t done = 0;
            while (done < n) {
                ssize_t written = write(out, buf + done, n - done);
                if (written <= 0) break;
                done += written;
            }
            EXPECT_EQ(n, done);
            if (done != n) break;
        }
        EXPECT_EQ(0, n);
        close(in);
        close(out);
        ASSERT_EQ(0, pipe(ready));
        ASSERT_EQ(0, pipe(release));
    }

    void TearDown() override {
        // Closing the release pipe also unblocks a helper after assertions fail.
        for (int& fd : release) {
            if (fd >= 0) close(fd);
            fd = -1;
        }
        if (child > 0) {
            int status;
            while (waitpid(child, &status, 0) < 0 && errno == EINTR) {}
        }
        for (int fd : ready) if (fd >= 0) close(fd);
        unlink(alias.c_str());
        unlink(image.c_str());
        rmdir(directory.c_str());
    }

    void start(bool fork_then_exec = false) {
        child = fork();
        ASSERT_GE(child, 0);
        if (child == 0) {
            close(ready[0]);
            close(release[1]);
            char output[24], input[24];
            snprintf(output, sizeof(output), "%d", ready[1]);
            snprintf(input, sizeof(input), "%d", release[0]);
            execl(image.c_str(), image.c_str(),
                  fork_then_exec ? "--fork-image" : "--hold-image",
                  output, input, self_path.c_str(), nullptr);
            _exit(errno);
        }
        close(ready[1]); ready[1] = -1;
        close(release[0]); release[0] = -1;
        ASSERT_TRUE(byte_io(ready[0], false));
        if (fork_then_exec) {
            ASSERT_TRUE(byte_io(ready[0], false));
        }
    }

    void finish() {
        ASSERT_TRUE(byte_io(release[1], true));
        int status = 0;
        ASSERT_EQ(child, waitpid(child, &status, 0));
        child = -1;
        ASSERT_TRUE(WIFEXITED(status));
        ASSERT_EQ(0, WEXITSTATUS(status));
    }

    int exec_errno() {
        pid_t pid = fork();
        if (pid == 0) {
            execl(image.c_str(), image.c_str(), "--exit-image", nullptr);
            _exit(errno);
        }
        if (pid < 0) return -1;
        int status = 0;
        if (waitpid(pid, &status, 0) != pid || !WIFEXITED(status)) return -1;
        return WEXITSTATUS(status);
    }

    void expect_open_error(const std::string& path, int flags) {
        errno = 0;
        int fd = open(path.c_str(), flags);
        int error = errno;
        if (fd >= 0) close(fd);
        EXPECT_EQ(-1, fd);
        EXPECT_EQ(ETXTBSY, error);
    }

    void expect_writable() {
        int fd = open(image.c_str(), O_WRONLY);
        ASSERT_GE(fd, 0) << strerror(errno);
        close(fd);
    }
};

TEST_F(ExecWriteAccess, WritableDescriptionPreventsExecUntilLastClose) {
    int fd = open(image.c_str(), O_WRONLY | O_CLOEXEC);
    ASSERT_GE(fd, 0);
    int duplicate = dup(fd);
    close(fd);
    EXPECT_EQ(ETXTBSY, exec_errno());
    close(duplicate);
    EXPECT_EQ(0, exec_errno());
}

TEST_F(ExecWriteAccess, RunningImageRejectsWritesTruncateAndHardlinkAlias) {
    ASSERT_EQ(0, link(image.c_str(), alias.c_str()));
    ASSERT_NO_FATAL_FAILURE(start());
    expect_open_error(image, O_WRONLY);
    expect_open_error(image, O_RDWR);
    expect_open_error(image, O_RDONLY | O_TRUNC);
    expect_open_error(alias, O_WRONLY);
    errno = 0;
    EXPECT_EQ(-1, truncate(image.c_str(), 0));
    EXPECT_EQ(ETXTBSY, errno);
    ASSERT_NO_FATAL_FAILURE(finish());
    expect_writable();
}

TEST_F(ExecWriteAccess, ForkKeepsDenyAfterParentExecAndLastExitReleasesIt) {
    // The exec'ed helper forks, then replaces its own image with the original
    // test binary. Only its child retains the copied executable's mm.
    ASSERT_NO_FATAL_FAILURE(start(true));
    expect_open_error(image, O_WRONLY);
    ASSERT_NO_FATAL_FAILURE(finish());
    expect_writable();
}

TEST_F(ExecWriteAccess, ProcMemReferenceDoesNotExtendExecutableDenyPastExit) {
    ASSERT_NO_FATAL_FAILURE(start());
    std::string mem_path = "/proc/" + std::to_string(child) + "/mem";
    int mem_fd = open(mem_path.c_str(), O_RDONLY);
    ASSERT_GE(mem_fd, 0) << strerror(errno);
    // Keep the external mm reference alive while the last task exits. Waiting
    // for the task, rather than closing /proc/pid/mem, must release exe deny.
    finish();
    expect_writable();
    close(mem_fd);
}

TEST_F(ExecWriteAccess, SharedMappingKeepsWritableDescriptionAfterClose) {
    int fd = open(image.c_str(), O_RDWR);
    ASSERT_GE(fd, 0);
    void* mapping = mmap(nullptr, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    close(fd);
    ASSERT_NE(MAP_FAILED, mapping);
    EXPECT_EQ(ETXTBSY, exec_errno());
    ASSERT_EQ(0, munmap(mapping, 4096));
    EXPECT_EQ(0, exec_errno());
}

TEST_F(ExecWriteAccess, RejectedFormatReleasesTemporaryDeny) {
    int fd = open(image.c_str(), O_WRONLY | O_TRUNC);
    ASSERT_GE(fd, 0);
    ASSERT_EQ(4, write(fd, "oops", 4));
    close(fd);
    EXPECT_EQ(ENOEXEC, exec_errno());
    expect_writable();
}

TEST_F(ExecWriteAccess, ScriptIsWritableWhileItsInterpreterRuns) {
    // Use the test binary itself as a shebang interpreter; no shell dependency.
    int fd = open(image.c_str(), O_WRONLY | O_TRUNC);
    ASSERT_GE(fd, 0);
    std::string script = "#!" + self_path + " --script-image\n";
    ASSERT_EQ(static_cast<ssize_t>(script.size()),
              write(fd, script.data(), script.size()));
    close(fd);
    ASSERT_NO_FATAL_FAILURE(start());
    expect_writable();
    ASSERT_NO_FATAL_FAILURE(finish());
}
}  // namespace

int main(int argc, char** argv) {
    if (argc == 2 && strcmp(argv[1], "--exit-image") == 0) return 0;
    if (argc == 7 && strcmp(argv[1], "--script-image") == 0) {
        // shebang adds its optional argument and script pathname before argv[1].
        return byte_io(atoi(argv[4]), true) && byte_io(atoi(argv[5]), false) ? 0 : 1;
    }
    if (argc == 5 && (strcmp(argv[1], "--hold-image") == 0 ||
                      strcmp(argv[1], "--fork-image") == 0)) {
        int output = atoi(argv[2]), input = atoi(argv[3]);
        if (strcmp(argv[1], "--fork-image") == 0) {
            pid_t pid = fork();
            if (pid < 0) return 2;
            if (pid > 0) {
                char descendant[24];
                snprintf(descendant, sizeof(descendant), "%d", pid);
                execl(argv[4], argv[4], "--reap-image", argv[2], descendant, nullptr);
                return 3;
            }
        }
        return byte_io(output, true) && byte_io(input, false) ? 0 : 1;
    }
    if (argc == 4 && strcmp(argv[1], "--reap-image") == 0) {
        if (!byte_io(atoi(argv[2]), true)) return 4;
        int status;
        return waitpid(atoi(argv[3]), &status, 0) > 0 &&
                       WIFEXITED(status) && WEXITSTATUS(status) == 0 ? 0 : 5;
    }
    // A failed exec must be reported as a test failure, not kill the runner.
    signal(SIGPIPE, SIG_IGN);
    char path[4096];
    ssize_t n = readlink("/proc/self/exe", path, sizeof(path) - 1);
    if (n < 0) return 1;
    path[n] = '\0';
    self_path = path;
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
