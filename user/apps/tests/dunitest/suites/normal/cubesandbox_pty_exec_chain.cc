#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <pty.h>
#include <sched.h>
#include <signal.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

#include <atomic>
#include <array>
#include <string>

namespace {

class UniqueFd {
public:
    UniqueFd() = default;
    explicit UniqueFd(int fd) : fd_(fd) {}
    UniqueFd(const UniqueFd&) = delete;
    UniqueFd& operator=(const UniqueFd&) = delete;

    UniqueFd(UniqueFd&& other) noexcept : fd_(other.fd_) { other.fd_ = -1; }

    UniqueFd& operator=(UniqueFd&& other) noexcept {
        if (this != &other) {
            reset();
            fd_ = other.fd_;
            other.fd_ = -1;
        }
        return *this;
    }

    ~UniqueFd() { reset(); }

    int get() const { return fd_; }

    void reset(int fd = -1) {
        if (fd_ >= 0) {
            close(fd_);
        }
        fd_ = fd;
    }

private:
    int fd_ = -1;
};

struct PtyPair {
    UniqueFd master;
    UniqueFd slave;
};

PtyPair OpenPty() {
    int master = -1;
    int slave = -1;
    if (openpty(&master, &slave, nullptr, nullptr, nullptr) < 0) {
        ADD_FAILURE() << "openpty failed: errno=" << errno << " (" << strerror(errno) << ")";
        return {};
    }
    return PtyPair{UniqueFd(master), UniqueFd(slave)};
}

void SetNonblock(int fd) {
    int flags = fcntl(fd, F_GETFL);
    ASSERT_GE(flags, 0) << "fcntl(F_GETFL) failed: errno=" << errno << " (" << strerror(errno)
                        << ")";
    ASSERT_EQ(0, fcntl(fd, F_SETFL, flags | O_NONBLOCK))
        << "fcntl(F_SETFL, O_NONBLOCK) failed: errno=" << errno << " (" << strerror(errno)
        << ")";
}

void SetRawByteMode(int fd) {
    struct termios term = {};
    ASSERT_EQ(0, tcgetattr(fd, &term))
        << "tcgetattr failed: errno=" << errno << " (" << strerror(errno) << ")";

    term.c_iflag = 0;
    term.c_oflag = 0;
    term.c_lflag = 0;
    term.c_cflag |= CS8;
    term.c_cc[VMIN] = 1;
    term.c_cc[VTIME] = 0;

    ASSERT_EQ(0, tcsetattr(fd, TCSANOW, &term))
        << "tcsetattr failed: errno=" << errno << " (" << strerror(errno) << ")";
}

bool WriteAll(int fd, const char* data, size_t len) {
    size_t written = 0;
    while (written < len) {
        ssize_t ret = write(fd, data + written, len - written);
        if (ret > 0) {
            written += static_cast<size_t>(ret);
            continue;
        }
        if (ret < 0 && errno == EINTR) {
            continue;
        }
        return false;
    }
    return true;
}

bool SetNonblockNoAssert(int fd) {
    int flags = fcntl(fd, F_GETFL);
    if (flags < 0) {
        return false;
    }
    return fcntl(fd, F_SETFL, flags | O_NONBLOCK) == 0;
}

bool WaitForChild(pid_t child, int* status, int rounds = 300) {
    for (int i = 0; i < rounds; ++i) {
        pid_t ret = waitpid(child, status, WNOHANG);
        if (ret == child) {
            return true;
        }
        if (ret < 0 && errno != EINTR) {
            return false;
        }
        usleep(10 * 1000);
    }
    return false;
}

void WriteReport(int report_fd, const std::string& report) {
    WriteAll(report_fd, report.c_str(), report.size());
}

void KillAndReap(pid_t child) {
    kill(child, SIGKILL);
    waitpid(child, nullptr, 0);
}

bool ReadUntilContains(int fd, const std::string& needle, std::string* output, int timeout_ms) {
    const size_t search_from = 0;
    int elapsed_ms = 0;
    while (elapsed_ms < timeout_ms) {
        struct pollfd pfd = {
            .fd = fd,
            .events = POLLIN | POLLERR | POLLHUP,
            .revents = 0,
        };

        sigset_t mask;
        sigemptyset(&mask);
        struct timespec ts = {
            .tv_sec = 0,
            .tv_nsec = 10 * 1000 * 1000,
        };
        int ret = ppoll(&pfd, 1, &ts, &mask);
        if (ret < 0) {
            if (errno == EINTR) {
                continue;
            }
            return false;
        }
        if (ret == 0) {
            elapsed_ms += 10;
            continue;
        }

        if ((pfd.revents & POLLIN) != 0) {
            char buf[256] = {};
            ssize_t n = read(fd, buf, sizeof(buf));
            if (n > 0) {
                output->append(buf, static_cast<size_t>(n));
                if (output->find(needle, search_from) != std::string::npos) {
                    return true;
                }
                continue;
            }
            if (n < 0 && errno == EINTR) {
                continue;
            }
            if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
                continue;
            }
            return false;
        }

        if ((pfd.revents & (POLLERR | POLLHUP)) != 0) {
            return output->find(needle, search_from) != std::string::npos;
        }

        usleep(10 * 1000);
        elapsed_ms += 10;
    }
    return false;
}

bool ReadUntilContainsFrom(int fd, const std::string& needle, std::string* output,
                           size_t search_from, int timeout_ms) {
    if (output->find(needle, search_from) != std::string::npos) {
        return true;
    }

    int elapsed_ms = 0;
    while (elapsed_ms < timeout_ms) {
        struct pollfd pfd = {
            .fd = fd,
            .events = POLLIN | POLLERR | POLLHUP,
            .revents = 0,
        };

        sigset_t mask;
        sigemptyset(&mask);
        struct timespec ts = {
            .tv_sec = 0,
            .tv_nsec = 10 * 1000 * 1000,
        };
        int ret = ppoll(&pfd, 1, &ts, &mask);
        if (ret < 0) {
            if (errno == EINTR) {
                continue;
            }
            return false;
        }
        if (ret == 0) {
            elapsed_ms += 10;
            continue;
        }

        if ((pfd.revents & POLLIN) != 0) {
            char buf[256] = {};
            ssize_t n = read(fd, buf, sizeof(buf));
            if (n > 0) {
                output->append(buf, static_cast<size_t>(n));
                if (output->find(needle, search_from) != std::string::npos) {
                    return true;
                }
                continue;
            }
            if (n < 0 && errno == EINTR) {
                continue;
            }
            if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
                continue;
            }
            return false;
        }

        if ((pfd.revents & (POLLERR | POLLHUP)) != 0) {
            return output->find(needle, search_from) != std::string::npos;
        }

        usleep(10 * 1000);
        elapsed_ms += 10;
    }
    return false;
}

bool WaitUntilAtomic(const std::atomic<int>& value, int expected_mask, int timeout_ms) {
    for (int elapsed_ms = 0; elapsed_ms < timeout_ms; elapsed_ms += 10) {
        if ((value.load(std::memory_order_acquire) & expected_mask) == expected_mask) {
            return true;
        }
        usleep(10 * 1000);
    }
    return false;
}

bool OutputLooksLikeShellPrompt(const std::string& output) {
    return output.find("# ") != std::string::npos || output.find("$ ") != std::string::npos;
}

void ExecUnameProgram() {
    execl("/bin/uname", "uname", "-a", nullptr);
    execl("/usr/bin/uname", "uname", "-a", nullptr);
    execl("/bin/busybox", "busybox", "uname", "-a", nullptr);
    _exit(127);
}

void ExecLsProgram() {
    execl("/bin/ls", "ls", "/", nullptr);
    execl("/usr/bin/ls", "ls", "/", nullptr);
    execl("/bin/busybox", "busybox", "ls", "/", nullptr);
    _exit(127);
}

std::string CollectFdUntilChildExit(int fd, pid_t child, int timeout_ms, int* status) {
    std::string output;
    for (int elapsed_ms = 0; elapsed_ms < timeout_ms; elapsed_ms += 10) {
        struct pollfd pfd = {
            .fd = fd,
            .events = POLLIN | POLLERR | POLLHUP,
            .revents = 0,
        };
        struct timespec ts = {
            .tv_sec = 0,
            .tv_nsec = 10 * 1000 * 1000,
        };
        sigset_t empty;
        sigemptyset(&empty);
        int pret = ppoll(&pfd, 1, &ts, &empty);
        if (pret < 0) {
            if (errno == EINTR) {
                continue;
            }
            ADD_FAILURE() << "ppoll failed while collecting child output: errno=" << errno << " ("
                          << strerror(errno) << ")";
            break;
        }

        if (pret > 0 && (pfd.revents & POLLIN) != 0) {
            std::array<char, 256> buf = {};
            for (;;) {
                ssize_t n = read(fd, buf.data(), buf.size());
                if (n > 0) {
                    output.append(buf.data(), static_cast<size_t>(n));
                    continue;
                }
                if (n < 0 && errno == EINTR) {
                    continue;
                }
                if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
                    break;
                }
                break;
            }
        }

        pid_t wait_ret = waitpid(child, status, WNOHANG);
        if (wait_ret == child) {
            for (;;) {
                std::array<char, 256> buf = {};
                ssize_t n = read(fd, buf.data(), buf.size());
                if (n > 0) {
                    output.append(buf.data(), static_cast<size_t>(n));
                    continue;
                }
                if (n < 0 && errno == EINTR) {
                    continue;
                }
                break;
            }
            return output;
        }
        if (wait_ret < 0 && errno != EINTR) {
            ADD_FAILURE() << "waitpid failed while collecting child output: errno=" << errno << " ("
                          << strerror(errno) << ")";
            break;
        }

        if (pret > 0 && (pfd.revents & (POLLERR | POLLHUP)) != 0) {
            continue;
        }
    }

    kill(child, SIGKILL);
    waitpid(child, status, 0);
    ADD_FAILURE() << "child did not exit within " << timeout_ms << " ms, captured: " << output;
    return output;
}

std::string ReadProcPrintk() {
    UniqueFd fd(open("/proc/sys/kernel/printk", O_RDONLY));
    if (fd.get() < 0) {
        ADD_FAILURE() << "open /proc/sys/kernel/printk failed: errno=" << errno << " ("
                      << strerror(errno) << ")";
        return {};
    }

    std::array<char, 64> buf = {};
    ssize_t n = read(fd.get(), buf.data(), buf.size() - 1);
    if (n < 0) {
        ADD_FAILURE() << "read /proc/sys/kernel/printk failed: errno=" << errno << " ("
                      << strerror(errno) << ")";
        return {};
    }
    return std::string(buf.data(), static_cast<size_t>(n));
}

void ExpectZeroLengthReadReturnsZero(int fd, const char* name) {
    errno = 0;
    EXPECT_EQ(0, read(fd, nullptr, 0))
        << name << " zero-length read should match Linux semantics, errno=" << errno << " ("
        << strerror(errno) << ")";
}

void ExpectZeroLengthWriteReturnsZero(int fd, const char* name) {
    errno = 0;
    EXPECT_EQ(0, write(fd, nullptr, 0))
        << name << " zero-length write should match Linux semantics, errno=" << errno << " ("
        << strerror(errno) << ")";
}

void WriteProcPrintk(const char* value) {
    UniqueFd fd(open("/proc/sys/kernel/printk", O_WRONLY));
    ASSERT_GE(fd.get(), 0) << "open /proc/sys/kernel/printk for write failed: errno=" << errno
                           << " (" << strerror(errno) << ")";
    ASSERT_EQ(static_cast<ssize_t>(strlen(value)), write(fd.get(), value, strlen(value)))
        << "write /proc/sys/kernel/printk failed: errno=" << errno << " (" << strerror(errno)
        << ")";
}

void BindSlaveAsControllingTty(int slave_fd) {
    if (setsid() < 0) {
        _exit(120);
    }
    if (ioctl(slave_fd, TIOCSCTTY, 0) != 0) {
        _exit(121);
    }
    if (tcgetpgrp(slave_fd) != getpgrp()) {
        _exit(122);
    }

    dup2(slave_fd, STDIN_FILENO);
    dup2(slave_fd, STDOUT_FILENO);
    dup2(slave_fd, STDERR_FILENO);
    if (slave_fd > STDERR_FILENO) {
        close(slave_fd);
    }
}

void ExecDefaultShellOnSlave(int slave_fd) {
    BindSlaveAsControllingTty(slave_fd);

    execl("/bin/sh", "sh", nullptr);
    execl("/usr/bin/sh", "sh", nullptr);
    execl("/bin/busybox", "busybox", "sh", nullptr);
    _exit(127);
}

void ExecBusyBoxShellOnSlave(int slave_fd) {
    BindSlaveAsControllingTty(slave_fd);

    execl("/bin/busybox", "busybox", "sh", nullptr);
    _exit(127);
}

void ExecUnameOnSlave(int slave_fd) {
    BindSlaveAsControllingTty(slave_fd);
    ExecUnameProgram();
}

void RunShellCommandSequence(void (*exec_shell)(int), const char* shell_name) {
    PtyPair pair = OpenPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);
    SetNonblock(pair.master.get());

    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        pair.master.reset();
        exec_shell(pair.slave.get());
    }

    pair.slave.reset();

    std::string output;
    ASSERT_TRUE(WriteAll(pair.master.get(), "ls /\n", strlen("ls /\n")))
        << "write ls failed through " << shell_name << ": errno=" << errno << " ("
        << strerror(errno) << ")";
    ASSERT_TRUE(ReadUntilContains(pair.master.get(), "bin", &output, 3000))
        << "did not observe ls output through " << shell_name << ", captured: " << output;

    ASSERT_TRUE(WriteAll(pair.master.get(), "uname -a\n", strlen("uname -a\n")))
        << "write uname failed through " << shell_name << ": errno=" << errno << " ("
        << strerror(errno) << ")";
    ASSERT_TRUE(ReadUntilContains(pair.master.get(), "Linux", &output, 5000))
        << "did not observe uname output through " << shell_name << ", captured: " << output;

    ASSERT_TRUE(WriteAll(pair.master.get(), "exit\n", strlen("exit\n")))
        << "write exit failed through " << shell_name << ": errno=" << errno << " ("
        << strerror(errno) << ")";

    int status = 0;
    if (!WaitForChild(child, &status)) {
        kill(child, SIGKILL);
        waitpid(child, nullptr, 0);
        FAIL() << shell_name << " did not exit after command sequence, captured: " << output;
    }
    ASSERT_TRUE(WIFEXITED(status) || WIFSIGNALED(status));
}

void RunRepeatedShellCommandSequence(void (*exec_shell)(int), const char* shell_name) {
    PtyPair pair = OpenPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);
    SetNonblock(pair.master.get());

    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        pair.master.reset();
        exec_shell(pair.slave.get());
    }

    pair.slave.reset();

    std::string output;
    for (int i = 0; i < 4; ++i) {
        size_t before_ls = output.size();
        ASSERT_TRUE(WriteAll(pair.master.get(), "ls /\n", strlen("ls /\n")))
            << "write ls failed through " << shell_name << " iteration " << i
            << ": errno=" << errno << " (" << strerror(errno) << ")";
        ASSERT_TRUE(ReadUntilContainsFrom(pair.master.get(), "bin", &output, before_ls, 3000))
            << "did not observe fresh ls output through " << shell_name << " iteration " << i
            << ", captured: " << output;

        size_t before_uname = output.size();
        ASSERT_TRUE(WriteAll(pair.master.get(), "uname -a\n", strlen("uname -a\n")))
            << "write uname failed through " << shell_name << " iteration " << i
            << ": errno=" << errno << " (" << strerror(errno) << ")";
        ASSERT_TRUE(
            ReadUntilContainsFrom(pair.master.get(), "Linux", &output, before_uname, 5000))
            << "did not observe fresh uname output through " << shell_name << " iteration " << i
            << ", captured: " << output;
    }

    ASSERT_TRUE(WriteAll(pair.master.get(), "exit\n", strlen("exit\n")))
        << "write exit failed through " << shell_name << ": errno=" << errno << " ("
        << strerror(errno) << ")";

    int status = 0;
    if (!WaitForChild(child, &status)) {
        kill(child, SIGKILL);
        waitpid(child, nullptr, 0);
        FAIL() << shell_name << " did not exit after repeated command sequence, captured: "
               << output;
    }
    ASSERT_TRUE(WIFEXITED(status) || WIFSIGNALED(status));
}

enum MockShimStage : int {
    kShimChildForked = 1 << 0,
    kShimStdinForwardStarted = 1 << 1,
    kShimStdinForwardFinished = 1 << 2,
    kShimSawLsOutput = 1 << 3,
    kShimSawUnameOutput = 1 << 4,
    kShimChildExited = 1 << 5,
};

void AppendStage(std::string* out, int stages, int bit, const char* name) {
    if ((stages & bit) == 0) {
        return;
    }
    if (!out->empty()) {
        out->append("|");
    }
    out->append(name);
}

std::string DescribeShimStages(int stages) {
    std::string out;
    AppendStage(&out, stages, kShimChildForked, "child-forked");
    AppendStage(&out, stages, kShimStdinForwardStarted, "stdin-forward-started");
    AppendStage(&out, stages, kShimStdinForwardFinished, "stdin-forward-finished");
    AppendStage(&out, stages, kShimSawLsOutput, "saw-ls-output");
    AppendStage(&out, stages, kShimSawUnameOutput, "saw-uname-output");
    AppendStage(&out, stages, kShimChildExited, "child-exited");
    return out.empty() ? "none" : out;
}

struct MockShimForwarder {
    int source_fd = -1;
    int pty_master_fd = -1;
    std::atomic<int>* stages = nullptr;
};

enum MockShimConcurrentStage : int {
    kConcurrentChildForked = 1 << 0,
    kConcurrentStdinForwardStarted = 1 << 1,
    kConcurrentStdinForwardFinished = 1 << 2,
    kConcurrentStdoutForwardStarted = 1 << 3,
    kConcurrentSawStartMarker = 1 << 4,
    kConcurrentSawLsOutput = 1 << 5,
    kConcurrentSawUnameOutput = 1 << 6,
    kConcurrentSawEndMarker = 1 << 7,
    kConcurrentChildExited = 1 << 8,
    kConcurrentStdoutForwardFinished = 1 << 9,
};

std::string DescribeConcurrentStages(int stages) {
    std::string out;
    AppendStage(&out, stages, kConcurrentChildForked, "child-forked");
    AppendStage(&out, stages, kConcurrentStdinForwardStarted, "stdin-forward-started");
    AppendStage(&out, stages, kConcurrentStdinForwardFinished, "stdin-forward-finished");
    AppendStage(&out, stages, kConcurrentStdoutForwardStarted, "stdout-forward-started");
    AppendStage(&out, stages, kConcurrentSawStartMarker, "saw-start-marker");
    AppendStage(&out, stages, kConcurrentSawLsOutput, "saw-ls-output");
    AppendStage(&out, stages, kConcurrentSawUnameOutput, "saw-uname-output");
    AppendStage(&out, stages, kConcurrentSawEndMarker, "saw-end-marker");
    AppendStage(&out, stages, kConcurrentChildExited, "child-exited");
    AppendStage(&out, stages, kConcurrentStdoutForwardFinished, "stdout-forward-finished");
    return out.empty() ? "none" : out;
}

struct MockShimStdoutForwarder {
    int pty_master_fd = -1;
    int client_stdout_fd = -1;
    std::atomic<int>* stages = nullptr;
    std::atomic<int>* child_exited = nullptr;
};

void* ForwardClientInputToPty(void* arg) {
    auto* forwarder = reinterpret_cast<MockShimForwarder*>(arg);
    forwarder->stages->fetch_or(kShimStdinForwardStarted, std::memory_order_release);

    std::array<char, 128> buf = {};
    for (;;) {
        ssize_t n = read(forwarder->source_fd, buf.data(), buf.size());
        if (n == 0) {
            break;
        }
        if (n < 0) {
            if (errno == EINTR) {
                continue;
            }
            return reinterpret_cast<void*>(1);
        }

        if (!WriteAll(forwarder->pty_master_fd, buf.data(), static_cast<size_t>(n))) {
            return reinterpret_cast<void*>(2);
        }
    }

    forwarder->stages->fetch_or(kShimStdinForwardFinished, std::memory_order_release);
    return nullptr;
}

void* ForwardPtyOutputToClient(void* arg) {
    auto* forwarder = reinterpret_cast<MockShimStdoutForwarder*>(arg);
    forwarder->stages->fetch_or(kConcurrentStdoutForwardStarted, std::memory_order_release);

    int idle_after_child_exit = 0;
    std::array<char, 256> buf = {};
    for (;;) {
        struct pollfd pfd = {
            .fd = forwarder->pty_master_fd,
            .events = POLLIN | POLLERR | POLLHUP,
            .revents = 0,
        };
        struct timespec ts = {
            .tv_sec = 0,
            .tv_nsec = 10 * 1000 * 1000,
        };
        sigset_t empty;
        sigemptyset(&empty);
        int ret = ppoll(&pfd, 1, &ts, &empty);
        if (ret < 0) {
            if (errno == EINTR) {
                continue;
            }
            close(forwarder->client_stdout_fd);
            return reinterpret_cast<void*>(1);
        }

        if (ret == 0) {
            if (forwarder->child_exited->load(std::memory_order_acquire) != 0 &&
                ++idle_after_child_exit >= 5) {
                break;
            }
            continue;
        }
        idle_after_child_exit = 0;

        if ((pfd.revents & POLLIN) != 0) {
            for (;;) {
                ssize_t n = read(forwarder->pty_master_fd, buf.data(), buf.size());
                if (n > 0) {
                    if (!WriteAll(forwarder->client_stdout_fd, buf.data(),
                                  static_cast<size_t>(n))) {
                        close(forwarder->client_stdout_fd);
                        return reinterpret_cast<void*>(2);
                    }
                    continue;
                }
                if (n == 0 || (n < 0 && errno == EIO)) {
                    break;
                }
                if (n < 0 && errno == EINTR) {
                    continue;
                }
                if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
                    break;
                }
                close(forwarder->client_stdout_fd);
                return reinterpret_cast<void*>(3);
            }
        }

        if ((pfd.revents & (POLLERR | POLLHUP)) != 0) {
            break;
        }
    }

    forwarder->stages->fetch_or(kConcurrentStdoutForwardFinished, std::memory_order_release);
    close(forwarder->client_stdout_fd);
    return nullptr;
}

void RunCubeShimLikeExecChain(void (*exec_shell)(int), const char* shell_name) {
    // Model the interactive Cube exec path at the kernel ABI boundary:
    // client stdin pipe -> shim forwarder thread -> PTY master -> shell on the
    // controlling PTY slave -> shim stdout polling on the PTY master.
    PtyPair pair = OpenPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);
    SetNonblock(pair.master.get());

    int client_stdin[2] = {-1, -1};
    ASSERT_EQ(0, pipe(client_stdin)) << strerror(errno);
    UniqueFd client_stdin_read(client_stdin[0]);
    UniqueFd client_stdin_write(client_stdin[1]);

    std::atomic<int> stages{0};

    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        client_stdin_read.reset();
        client_stdin_write.reset();
        pair.master.reset();
        exec_shell(pair.slave.get());
    }

    stages.fetch_or(kShimChildForked, std::memory_order_release);
    pair.slave.reset();

    MockShimForwarder forwarder = {
        .source_fd = client_stdin_read.get(),
        .pty_master_fd = pair.master.get(),
        .stages = &stages,
    };

    pthread_t stdin_thread = {};
    ASSERT_EQ(0, pthread_create(&stdin_thread, nullptr, ForwardClientInputToPty, &forwarder))
        << strerror(errno);

    ASSERT_TRUE(WaitUntilAtomic(stages, kShimStdinForwardStarted, 1000))
        << "stdin forwarder did not start for " << shell_name;

    constexpr char kCommands[] = "ls /\nuname -a\nexit\n";
    ASSERT_TRUE(WriteAll(client_stdin_write.get(), kCommands, strlen(kCommands)))
        << "client stdin write failed for " << shell_name << ": errno=" << errno << " ("
        << strerror(errno) << ")";
    client_stdin_write.reset();

    std::string output;
    int elapsed_ms = 0;
    while (elapsed_ms < 5000) {
        struct pollfd pfd = {
            .fd = pair.master.get(),
            .events = POLLIN | POLLERR | POLLHUP,
            .revents = 0,
        };
        struct timespec ts = {
            .tv_sec = 0,
            .tv_nsec = 10 * 1000 * 1000,
        };
        sigset_t empty;
        sigemptyset(&empty);
        int ret = ppoll(&pfd, 1, &ts, &empty);
        if (ret < 0) {
            if (errno == EINTR) {
                continue;
            }
            FAIL() << "ppoll failed in mock shim stdout loop for " << shell_name
                   << ": errno=" << errno << " (" << strerror(errno) << ")";
        }
        if (ret == 0) {
            elapsed_ms += 10;
            continue;
        }

        if ((pfd.revents & POLLIN) != 0) {
            std::array<char, 256> buf = {};
            ssize_t n = read(pair.master.get(), buf.data(), buf.size());
            if (n > 0) {
                output.append(buf.data(), static_cast<size_t>(n));
                if (output.find("bin") != std::string::npos) {
                    stages.fetch_or(kShimSawLsOutput, std::memory_order_release);
                }
                if (output.find("Linux") != std::string::npos) {
                    stages.fetch_or(kShimSawUnameOutput, std::memory_order_release);
                    break;
                }
                continue;
            }
            if (n < 0 && (errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK)) {
                continue;
            }
        }

        if ((pfd.revents & (POLLERR | POLLHUP)) != 0) {
            break;
        }
    }

    void* thread_result = nullptr;
    ASSERT_EQ(0, pthread_join(stdin_thread, &thread_result)) << strerror(errno);
    EXPECT_EQ(nullptr, thread_result) << "stdin forwarder failed for " << shell_name;

    int status = 0;
    if (!WaitForChild(child, &status)) {
        int observed = stages.load(std::memory_order_acquire);
        kill(child, SIGKILL);
        waitpid(child, nullptr, 0);
        FAIL() << "mock shim child did not exit for " << shell_name << ", stages=0x" << std::hex
               << observed << " (" << DescribeShimStages(observed) << "), captured: " << output;
    }
    stages.fetch_or(kShimChildExited, std::memory_order_release);

    constexpr int kExpectedStages = kShimChildForked | kShimStdinForwardStarted |
                                    kShimStdinForwardFinished | kShimSawLsOutput |
                                    kShimSawUnameOutput | kShimChildExited;
    int observed = stages.load(std::memory_order_acquire);
    EXPECT_EQ(kExpectedStages, observed)
        << "mock shim chain did not reach all stages for " << shell_name
        << ", stages=0x" << std::hex << observed << " (" << DescribeShimStages(observed) << ")"
        << ", captured output: " << output;
    ASSERT_TRUE(WIFEXITED(status) || WIFSIGNALED(status));
}

void RunCubeShimLikeConcurrentForwarders(void (*exec_shell)(int), const char* shell_name) {
    PtyPair pair = OpenPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);
    SetNonblock(pair.master.get());

    int client_stdin[2] = {-1, -1};
    ASSERT_EQ(0, pipe(client_stdin)) << strerror(errno);
    UniqueFd client_stdin_read(client_stdin[0]);
    UniqueFd client_stdin_write(client_stdin[1]);

    int client_stdout[2] = {-1, -1};
    ASSERT_EQ(0, pipe(client_stdout)) << strerror(errno);
    UniqueFd client_stdout_read(client_stdout[0]);
    int client_stdout_write = client_stdout[1];
    SetNonblock(client_stdout_read.get());

    std::atomic<int> stages{0};
    std::atomic<int> child_exited{0};

    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        client_stdin_read.reset();
        client_stdin_write.reset();
        client_stdout_read.reset();
        close(client_stdout_write);
        pair.master.reset();
        exec_shell(pair.slave.get());
    }

    stages.fetch_or(kConcurrentChildForked, std::memory_order_release);
    pair.slave.reset();

    MockShimForwarder stdin_forwarder = {
        .source_fd = client_stdin_read.get(),
        .pty_master_fd = pair.master.get(),
        .stages = &stages,
    };
    MockShimStdoutForwarder stdout_forwarder = {
        .pty_master_fd = pair.master.get(),
        .client_stdout_fd = client_stdout_write,
        .stages = &stages,
        .child_exited = &child_exited,
    };

    pthread_t stdin_thread = {};
    ASSERT_EQ(0, pthread_create(&stdin_thread, nullptr, ForwardClientInputToPty, &stdin_forwarder))
        << strerror(errno);

    pthread_t stdout_thread = {};
    ASSERT_EQ(0,
              pthread_create(&stdout_thread, nullptr, ForwardPtyOutputToClient, &stdout_forwarder))
        << strerror(errno);

    ASSERT_TRUE(WaitUntilAtomic(stages, kShimStdinForwardStarted, 1000))
        << "stdin forwarder did not start for " << shell_name;
    ASSERT_TRUE(WaitUntilAtomic(stages, kConcurrentStdoutForwardStarted, 1000))
        << "stdout forwarder did not start for " << shell_name;

    constexpr char kCommands[] =
        "echo cube-atomic-start\n"
        "ls /\n"
        "uname -a\n"
        "echo cube-atomic-end\n"
        "exit\n";
    ASSERT_TRUE(WriteAll(client_stdin_write.get(), kCommands, strlen(kCommands)))
        << "client stdin write failed for " << shell_name << ": errno=" << errno << " ("
        << strerror(errno) << ")";
    client_stdin_write.reset();

    std::string output;
    ASSERT_TRUE(ReadUntilContainsFrom(client_stdout_read.get(), "cube-atomic-start", &output,
                                      0, 3000))
        << "did not observe start marker for " << shell_name << ", captured: " << output;
    stages.fetch_or(kConcurrentSawStartMarker, std::memory_order_release);

    ASSERT_TRUE(ReadUntilContainsFrom(client_stdout_read.get(), "bin", &output, 0, 3000))
        << "did not observe ls output for " << shell_name << ", captured: " << output;
    stages.fetch_or(kConcurrentSawLsOutput, std::memory_order_release);

    ASSERT_TRUE(ReadUntilContainsFrom(client_stdout_read.get(), "Linux", &output, 0, 5000))
        << "did not observe uname output for " << shell_name << ", captured: " << output;
    stages.fetch_or(kConcurrentSawUnameOutput, std::memory_order_release);

    ASSERT_TRUE(ReadUntilContainsFrom(client_stdout_read.get(), "cube-atomic-end", &output,
                                      0, 3000))
        << "did not observe end marker for " << shell_name << ", captured: " << output;
    stages.fetch_or(kConcurrentSawEndMarker, std::memory_order_release);

    void* stdin_result = nullptr;
    ASSERT_EQ(0, pthread_join(stdin_thread, &stdin_result)) << strerror(errno);
    EXPECT_EQ(nullptr, stdin_result) << "stdin forwarder failed for " << shell_name;

    int status = 0;
    if (!WaitForChild(child, &status)) {
        int observed = stages.load(std::memory_order_acquire);
        kill(child, SIGKILL);
        waitpid(child, nullptr, 0);
        child_exited.store(1, std::memory_order_release);
        pthread_join(stdout_thread, nullptr);
        FAIL() << "child did not exit for " << shell_name << ", stages=0x" << std::hex
               << observed << " (" << DescribeConcurrentStages(observed)
               << "), captured: " << output;
    }
    stages.fetch_or(kConcurrentChildExited, std::memory_order_release);
    child_exited.store(1, std::memory_order_release);

    void* stdout_result = nullptr;
    ASSERT_EQ(0, pthread_join(stdout_thread, &stdout_result)) << strerror(errno);
    EXPECT_EQ(nullptr, stdout_result) << "stdout forwarder failed for " << shell_name;

    constexpr int kExpectedStages =
        kConcurrentChildForked | kConcurrentStdinForwardStarted |
        kConcurrentStdinForwardFinished | kConcurrentStdoutForwardStarted |
        kConcurrentSawStartMarker | kConcurrentSawLsOutput | kConcurrentSawUnameOutput |
        kConcurrentSawEndMarker | kConcurrentChildExited | kConcurrentStdoutForwardFinished;
    int observed = stages.load(std::memory_order_acquire);
    EXPECT_EQ(kExpectedStages, observed)
        << "concurrent shim chain did not reach all stages for " << shell_name
        << ", stages=0x" << std::hex << observed << " ("
        << DescribeConcurrentStages(observed) << ")"
        << ", captured output: " << output;
    ASSERT_TRUE(WIFEXITED(status) || WIFSIGNALED(status));
}

void RunCubeShimLikeByteStreamInput(void (*exec_shell)(int), const char* shell_name) {
    PtyPair pair = OpenPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);
    SetNonblock(pair.master.get());

    int client_stdin[2] = {-1, -1};
    ASSERT_EQ(0, pipe(client_stdin)) << strerror(errno);
    UniqueFd client_stdin_read(client_stdin[0]);
    UniqueFd client_stdin_write(client_stdin[1]);

    int client_stdout[2] = {-1, -1};
    ASSERT_EQ(0, pipe(client_stdout)) << strerror(errno);
    UniqueFd client_stdout_read(client_stdout[0]);
    int client_stdout_write = client_stdout[1];
    SetNonblock(client_stdout_read.get());

    std::atomic<int> stages{0};
    std::atomic<int> child_exited{0};

    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        client_stdin_read.reset();
        client_stdin_write.reset();
        client_stdout_read.reset();
        close(client_stdout_write);
        pair.master.reset();
        exec_shell(pair.slave.get());
    }

    stages.fetch_or(kConcurrentChildForked, std::memory_order_release);
    pair.slave.reset();

    MockShimForwarder stdin_forwarder = {
        .source_fd = client_stdin_read.get(),
        .pty_master_fd = pair.master.get(),
        .stages = &stages,
    };
    MockShimStdoutForwarder stdout_forwarder = {
        .pty_master_fd = pair.master.get(),
        .client_stdout_fd = client_stdout_write,
        .stages = &stages,
        .child_exited = &child_exited,
    };

    pthread_t stdin_thread = {};
    ASSERT_EQ(0, pthread_create(&stdin_thread, nullptr, ForwardClientInputToPty, &stdin_forwarder))
        << strerror(errno);

    pthread_t stdout_thread = {};
    ASSERT_EQ(0,
              pthread_create(&stdout_thread, nullptr, ForwardPtyOutputToClient, &stdout_forwarder))
        << strerror(errno);

    ASSERT_TRUE(WaitUntilAtomic(stages, kShimStdinForwardStarted, 1000))
        << "stdin forwarder did not start for " << shell_name;
    ASSERT_TRUE(WaitUntilAtomic(stages, kConcurrentStdoutForwardStarted, 1000))
        << "stdout forwarder did not start for " << shell_name;

    constexpr char kCommands[] =
        "echo cube-byte-start\n"
        "echo cube-byte-done\n"
        "exit\n";
    for (size_t i = 0; i < sizeof(kCommands) - 1; ++i) {
        ASSERT_TRUE(WriteAll(client_stdin_write.get(), &kCommands[i], 1))
            << "client byte write failed for " << shell_name << " at byte " << i
            << ": errno=" << errno << " (" << strerror(errno) << ")";
        usleep(1000);
    }
    client_stdin_write.reset();

    std::string output;
    ASSERT_TRUE(ReadUntilContainsFrom(client_stdout_read.get(), "cube-byte-start", &output,
                                      0, 5000))
        << "did not observe byte-stream start marker for " << shell_name
        << ", captured: " << output;
    ASSERT_TRUE(ReadUntilContainsFrom(client_stdout_read.get(), "cube-byte-done", &output,
                                      0, 5000))
        << "did not observe byte-stream done marker for " << shell_name
        << ", captured: " << output;

    void* stdin_result = nullptr;
    ASSERT_EQ(0, pthread_join(stdin_thread, &stdin_result)) << strerror(errno);
    EXPECT_EQ(nullptr, stdin_result) << "stdin forwarder failed for " << shell_name;

    int status = 0;
    if (!WaitForChild(child, &status)) {
        int observed = stages.load(std::memory_order_acquire);
        kill(child, SIGKILL);
        waitpid(child, nullptr, 0);
        child_exited.store(1, std::memory_order_release);
        pthread_join(stdout_thread, nullptr);
        FAIL() << "byte-stream child did not exit for " << shell_name << ", stages=0x"
               << std::hex << observed << " (" << DescribeConcurrentStages(observed)
               << "), captured: " << output;
    }
    child_exited.store(1, std::memory_order_release);

    void* stdout_result = nullptr;
    ASSERT_EQ(0, pthread_join(stdout_thread, &stdout_result)) << strerror(errno);
    EXPECT_EQ(nullptr, stdout_result) << "stdout forwarder failed for " << shell_name;
    ASSERT_TRUE(WIFEXITED(status) || WIFSIGNALED(status));
}

int RunVforkExecLsAndReport(int report_fd) {
    int stdout_pipe[2] = {-1, -1};
    if (pipe(stdout_pipe) != 0) {
        return 10;
    }

    pid_t child = vfork();
    if (child == 0) {
        close(stdout_pipe[0]);
        dup2(stdout_pipe[1], STDOUT_FILENO);
        dup2(stdout_pipe[1], STDERR_FILENO);
        if (stdout_pipe[1] > STDERR_FILENO) {
            close(stdout_pipe[1]);
        }
        ExecLsProgram();
    }
    if (child < 0) {
        return 11;
    }

    close(stdout_pipe[1]);

    std::string output;
    std::array<char, 256> buf = {};
    for (;;) {
        ssize_t n = read(stdout_pipe[0], buf.data(), buf.size());
        if (n > 0) {
            output.append(buf.data(), static_cast<size_t>(n));
            continue;
        }
        if (n < 0 && errno == EINTR) {
            continue;
        }
        break;
    }
    close(stdout_pipe[0]);

    int status = 0;
    if (waitpid(child, &status, 0) != child) {
        return 12;
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        return 13;
    }
    if (output.find("bin") == std::string::npos) {
        return 14;
    }
    char ok = '1';
    if (write(report_fd, &ok, 1) != 1) {
        return 15;
    }
    return 0;
}

int RunPidNamespaceInteractiveShellAndReport(int report_fd) {
    int master = -1;
    int slave = -1;
    if (openpty(&master, &slave, nullptr, nullptr, nullptr) != 0) {
        WriteReport(report_fd, "openpty failed");
        return 30;
    }
    if (!SetNonblockNoAssert(master)) {
        WriteReport(report_fd, "set nonblock failed");
        close(master);
        close(slave);
        return 31;
    }

    pid_t shell = fork();
    if (shell == 0) {
        close(master);
        ExecDefaultShellOnSlave(slave);
    }
    if (shell < 0) {
        WriteReport(report_fd, "fork shell failed");
        close(master);
        close(slave);
        return 32;
    }

    close(slave);

    std::string output;
    ReadUntilContains(master, " ", &output, 1000);

    constexpr char kCommands[] =
        "echo cube-pidns-start\n"
        "ls /\n"
        "uname -a\n"
        "echo cube-pidns-end\n"
        "exit\n";
    if (!WriteAll(master, kCommands, strlen(kCommands))) {
        WriteReport(report_fd, "write commands failed: " + std::to_string(errno));
        kill(shell, SIGKILL);
        waitpid(shell, nullptr, 0);
        close(master);
        return 33;
    }

    if (!ReadUntilContains(master, "cube-pidns-start", &output, 3000)) {
        WriteReport(report_fd, "missing start marker: " + output);
        kill(shell, SIGKILL);
        waitpid(shell, nullptr, 0);
        close(master);
        return 34;
    }
    if (!ReadUntilContains(master, "bin", &output, 3000)) {
        WriteReport(report_fd, "missing ls output: " + output);
        kill(shell, SIGKILL);
        waitpid(shell, nullptr, 0);
        close(master);
        return 35;
    }
    if (!ReadUntilContains(master, "Linux", &output, 5000)) {
        WriteReport(report_fd, "missing uname output: " + output);
        kill(shell, SIGKILL);
        waitpid(shell, nullptr, 0);
        close(master);
        return 36;
    }
    if (!ReadUntilContains(master, "cube-pidns-end", &output, 3000)) {
        WriteReport(report_fd, "missing end marker: " + output);
        kill(shell, SIGKILL);
        waitpid(shell, nullptr, 0);
        close(master);
        return 37;
    }

    int status = 0;
    if (!WaitForChild(shell, &status)) {
        WriteReport(report_fd, "shell did not exit: " + output);
        kill(shell, SIGKILL);
        waitpid(shell, nullptr, 0);
        close(master);
        return 38;
    }
    close(master);

    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        WriteReport(report_fd, "shell bad status: " + std::to_string(status) + " output: " +
                                   output);
        return 39;
    }

    WriteReport(report_fd, "OK");
    return 0;
}

void ExpectVforkExecLsCompletesInChildPidNamespace() {
    int report_pipe[2] = {-1, -1};
    ASSERT_EQ(0, pipe(report_pipe)) << strerror(errno);
    UniqueFd report_read(report_pipe[0]);
    UniqueFd report_write(report_pipe[1]);
    SetNonblock(report_read.get());

    pid_t outer = fork();
    ASSERT_GE(outer, 0) << strerror(errno);
    if (outer == 0) {
        report_read.reset();
        if (unshare(CLONE_NEWPID) != 0) {
            _exit(errno == ENOSYS || errno == EINVAL ? 77 : 20);
        }

        pid_t init = fork();
        if (init == 0) {
            int rc = RunVforkExecLsAndReport(report_write.get());
            _exit(rc);
        }
        if (init < 0) {
            _exit(21);
        }

        int status = 0;
        if (waitpid(init, &status, 0) != init) {
            _exit(22);
        }
        if (!WIFEXITED(status)) {
            _exit(23);
        }
        _exit(WEXITSTATUS(status));
    }

    report_write.reset();

    std::string report;
    int status = 0;
    bool exited = false;
    for (int elapsed_ms = 0; elapsed_ms < 5000; elapsed_ms += 10) {
        char ch = 0;
        ssize_t n = read(report_read.get(), &ch, 1);
        if (n == 1) {
            report.push_back(ch);
        }
        pid_t ret = waitpid(outer, &status, WNOHANG);
        if (ret == outer) {
            exited = true;
            break;
        }
        ASSERT_FALSE(ret < 0 && errno != EINTR) << strerror(errno);
        usleep(10 * 1000);
    }

    if (!exited) {
        KillAndReap(outer);
        FAIL() << "vfork+exec /bin/ls did not finish inside a child PID namespace";
    }
    ASSERT_TRUE(WIFEXITED(status)) << "outer status=" << status;
    if (WEXITSTATUS(status) == 77) {
        GTEST_SKIP() << "CLONE_NEWPID is not available";
    }
    ASSERT_EQ(0, WEXITSTATUS(status)) << "outer exit status=" << WEXITSTATUS(status);
    EXPECT_EQ("1", report);
}

void ExpectInteractiveShellCommandsCompleteInChildPidNamespace() {
    int report_pipe[2] = {-1, -1};
    ASSERT_EQ(0, pipe(report_pipe)) << strerror(errno);
    UniqueFd report_read(report_pipe[0]);
    UniqueFd report_write(report_pipe[1]);
    SetNonblock(report_read.get());

    pid_t outer = fork();
    ASSERT_GE(outer, 0) << strerror(errno);
    if (outer == 0) {
        report_read.reset();
        if (unshare(CLONE_NEWPID) != 0) {
            _exit(errno == ENOSYS || errno == EINVAL ? 77 : 40);
        }

        pid_t init = fork();
        if (init == 0) {
            int rc = RunPidNamespaceInteractiveShellAndReport(report_write.get());
            _exit(rc);
        }
        if (init < 0) {
            _exit(41);
        }

        int status = 0;
        if (waitpid(init, &status, 0) != init) {
            _exit(42);
        }
        if (!WIFEXITED(status)) {
            _exit(43);
        }
        _exit(WEXITSTATUS(status));
    }

    report_write.reset();

    std::string report;
    int status = 0;
    bool exited = false;
    for (int elapsed_ms = 0; elapsed_ms < 10000; elapsed_ms += 10) {
        std::array<char, 256> buf = {};
        ssize_t n = read(report_read.get(), buf.data(), buf.size());
        if (n > 0) {
            report.append(buf.data(), static_cast<size_t>(n));
        }

        pid_t ret = waitpid(outer, &status, WNOHANG);
        if (ret == outer) {
            exited = true;
            break;
        }
        ASSERT_FALSE(ret < 0 && errno != EINTR) << strerror(errno);
        usleep(10 * 1000);
    }

    if (!exited) {
        KillAndReap(outer);
        FAIL() << "interactive shell commands did not finish inside a child PID namespace, "
                  "captured report: "
               << report;
    }
    ASSERT_TRUE(WIFEXITED(status)) << "outer status=" << status << ", report: " << report;
    if (WEXITSTATUS(status) == 77) {
        GTEST_SKIP() << "CLONE_NEWPID is not available";
    }
    ASSERT_EQ(0, WEXITSTATUS(status))
        << "outer exit status=" << WEXITSTATUS(status) << ", report: " << report;
    EXPECT_EQ("OK", report);
}

TEST(CubeSandboxPtyExecChain, BlockingPtyMasterReadDoesNotBlockConcurrentWrite) {
    PtyPair pair = OpenPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);

    pid_t shell = fork();
    ASSERT_GE(shell, 0) << strerror(errno);
    if (shell == 0) {
        pair.master.reset();
        ExecDefaultShellOnSlave(pair.slave.get());
    }

    pair.slave.reset();

    int captured_pipe[2] = {-1, -1};
    ASSERT_EQ(0, pipe(captured_pipe)) << strerror(errno);
    UniqueFd captured_read(captured_pipe[0]);
    UniqueFd captured_write(captured_pipe[1]);
    SetNonblock(captured_read.get());

    pid_t reader = fork();
    ASSERT_GE(reader, 0) << strerror(errno);
    if (reader == 0) {
        captured_read.reset();

        std::array<char, 256> buf = {};
        for (;;) {
            ssize_t n = read(pair.master.get(), buf.data(), buf.size());
            if (n > 0) {
                if (!WriteAll(captured_write.get(), buf.data(), static_cast<size_t>(n))) {
                    _exit(2);
                }
                continue;
            }
            if (n < 0 && errno == EINTR) {
                continue;
            }
            _exit(n == 0 ? 0 : 1);
        }
    }

    captured_write.reset();

    std::string output;
    ASSERT_TRUE(ReadUntilContainsFrom(captured_read.get(), " ", &output, 0, 3000))
        << "reader did not capture any shell output";
    ASSERT_TRUE(OutputLooksLikeShellPrompt(output)) << "initial prompt not captured: " << output;

    constexpr char kEchoCommand[] = "echo cube-blocking-read-ok\n";
    ASSERT_TRUE(WriteAll(pair.master.get(), kEchoCommand, strlen(kEchoCommand)))
        << "write to PTY master failed while reader is blocked: errno=" << errno << " ("
        << strerror(errno) << ")";

    const size_t after_prompt = output.size();
    ASSERT_TRUE(ReadUntilContainsFrom(captured_read.get(), "cube-blocking-read-ok", &output,
                                      after_prompt, 5000))
        << "blocking reader did not observe command output after concurrent write, captured: "
        << output;

    ASSERT_TRUE(WriteAll(pair.master.get(), "exit\n", strlen("exit\n")))
        << "write exit failed: errno=" << errno << " (" << strerror(errno) << ")";

    int shell_status = 0;
    if (!WaitForChild(shell, &shell_status)) {
        kill(shell, SIGKILL);
        waitpid(shell, nullptr, 0);
        kill(reader, SIGKILL);
        waitpid(reader, nullptr, 0);
        FAIL() << "shell did not exit after blocking-read test, captured: " << output;
    }

    pair.master.reset();

    int reader_status = 0;
    if (!WaitForChild(reader, &reader_status)) {
        kill(reader, SIGKILL);
        waitpid(reader, nullptr, 0);
        FAIL() << "PTY master reader did not exit after shell close, captured: " << output;
    }

    ASSERT_TRUE(WIFEXITED(shell_status) || WIFSIGNALED(shell_status));
    ASSERT_TRUE(WIFEXITED(reader_status) || WIFSIGNALED(reader_status));
}

TEST(CubeSandboxPtyExecChain, VforkExecLsCompletesInChildPidNamespace) {
    ExpectVforkExecLsCompletesInChildPidNamespace();
}

TEST(CubeSandboxPtyExecChain, InteractiveShellCommandsCompleteInChildPidNamespace) {
    ExpectInteractiveShellCommandsCompleteInChildPidNamespace();
}

TEST(CubeSandboxPtyExecChain, PipeExecCollectsUnameStdout) {
    int stdout_pipe[2] = {-1, -1};
    ASSERT_EQ(0, pipe(stdout_pipe)) << strerror(errno);
    UniqueFd stdout_read(stdout_pipe[0]);
    UniqueFd stdout_write(stdout_pipe[1]);
    SetNonblock(stdout_read.get());

    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        stdout_read.reset();
        dup2(stdout_write.get(), STDOUT_FILENO);
        dup2(stdout_write.get(), STDERR_FILENO);
        if (stdout_write.get() > STDERR_FILENO) {
            stdout_write.reset();
        }
        ExecUnameProgram();
    }

    stdout_write.reset();

    int status = 0;
    std::string output = CollectFdUntilChildExit(stdout_read.get(), child, 5000, &status);
    ASSERT_TRUE(WIFEXITED(status)) << "uname status=" << status << ", output: " << output;
    ASSERT_EQ(0, WEXITSTATUS(status)) << "uname output: " << output;
    EXPECT_NE(std::string::npos, output.find("Linux")) << "uname output: " << output;
}

TEST(CubeSandboxPtyExecChain, ZeroLengthPipeIoMatchesLinux) {
    int fds[2] = {-1, -1};
    ASSERT_EQ(0, pipe(fds)) << strerror(errno);
    UniqueFd read_end(fds[0]);
    UniqueFd write_end(fds[1]);

    ExpectZeroLengthReadReturnsZero(read_end.get(), "pipe read end");
    ExpectZeroLengthWriteReturnsZero(write_end.get(), "pipe write end");

    ASSERT_TRUE(WriteAll(write_end.get(), "x", 1))
        << "pipe write failed after zero-length operations: errno=" << errno << " ("
        << strerror(errno) << ")";
    char observed = 0;
    ASSERT_EQ(1, read(read_end.get(), &observed, 1)) << strerror(errno);
    EXPECT_EQ('x', observed);
}

TEST(CubeSandboxPtyExecChain, PtyExecDirectUnameEmitsOutputAndExits) {
    PtyPair pair = OpenPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);
    SetNonblock(pair.master.get());

    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        pair.master.reset();
        ExecUnameOnSlave(pair.slave.get());
    }

    pair.slave.reset();

    int status = 0;
    std::string output = CollectFdUntilChildExit(pair.master.get(), child, 5000, &status);
    ASSERT_TRUE(WIFEXITED(status)) << "uname status=" << status << ", output: " << output;
    ASSERT_EQ(0, WEXITSTATUS(status)) << "uname output: " << output;
    EXPECT_NE(std::string::npos, output.find("Linux")) << "uname output: " << output;
}

TEST(CubeSandboxPtyExecChain, ZeroLengthPtyIoMatchesLinux) {
    PtyPair pair = OpenPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);
    SetRawByteMode(pair.slave.get());

    ExpectZeroLengthReadReturnsZero(pair.master.get(), "pty master");
    ExpectZeroLengthReadReturnsZero(pair.slave.get(), "pty slave");
    ExpectZeroLengthWriteReturnsZero(pair.master.get(), "pty master");
    ExpectZeroLengthWriteReturnsZero(pair.slave.get(), "pty slave");

    ASSERT_TRUE(WriteAll(pair.master.get(), "z", 1))
        << "pty master write failed after zero-length operations: errno=" << errno << " ("
        << strerror(errno) << ")";
    char observed = 0;
    ASSERT_EQ(1, read(pair.slave.get(), &observed, 1)) << strerror(errno);
    EXPECT_EQ('z', observed);
}

TEST(CubeSandboxPtyExecChain, DefaultPrintkLevelDoesNotEnableDebugConsoleSpam) {
    ASSERT_EQ("7\t4\t1\t7\n", ReadProcPrintk());

    WriteProcPrintk("8\n");
    EXPECT_EQ("8\t4\t1\t7\n", ReadProcPrintk());

    WriteProcPrintk("7\n");
    EXPECT_EQ("7\t4\t1\t7\n", ReadProcPrintk());
}

TEST(CubeSandboxPtyExecChain, RawPtyByteReadsSurviveSigmaskAndPpoll) {
    PtyPair pair = OpenPty();
    ASSERT_GE(pair.master.get(), 0);
    ASSERT_GE(pair.slave.get(), 0);
    SetRawByteMode(pair.slave.get());

    int ready_pipe[2] = {-1, -1};
    ASSERT_EQ(0, pipe(ready_pipe)) << strerror(errno);

    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        close(ready_pipe[0]);
        pair.master.reset();

        char ready = 'r';
        if (write(ready_pipe[1], &ready, 1) != 1) {
            _exit(2);
        }

        constexpr char kExpected[] = "ls\nuname -a\n";
        for (size_t i = 0; i < sizeof(kExpected) - 1; ++i) {
            sigset_t mask;
            sigemptyset(&mask);
            sigaddset(&mask, SIGCHLD);
            if (sigprocmask(SIG_SETMASK, &mask, nullptr) < 0) {
                _exit(3);
            }

            struct pollfd pfd = {
                .fd = pair.slave.get(),
                .events = POLLIN | POLLERR | POLLHUP,
                .revents = 0,
            };
            sigset_t empty;
            sigemptyset(&empty);
            int pret = ppoll(&pfd, 1, nullptr, &empty);
            if (pret <= 0 || (pfd.revents & POLLIN) == 0) {
                _exit(4);
            }

            if (sigprocmask(SIG_SETMASK, &empty, nullptr) < 0) {
                _exit(5);
            }

            char ch = 0;
            ssize_t n = read(pair.slave.get(), &ch, 1);
            if (n != 1 || ch != kExpected[i]) {
                _exit(6);
            }
        }
        _exit(0);
    }

    close(ready_pipe[1]);
    pair.slave.reset();

    char ready = 0;
    ASSERT_EQ(1, read(ready_pipe[0], &ready, 1)) << strerror(errno);
    ASSERT_EQ('r', ready);
    close(ready_pipe[0]);

    ASSERT_TRUE(WriteAll(pair.master.get(), "ls\nuname -a\n", strlen("ls\nuname -a\n")))
        << "write master failed: errno=" << errno << " (" << strerror(errno) << ")";

    int status = 0;
    if (!WaitForChild(child, &status)) {
        kill(child, SIGKILL);
        waitpid(child, nullptr, 0);
        FAIL() << "raw pty byte reader timed out";
    }
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(CubeSandboxPtyExecChain, ShellRunsLsThenUnameThroughControllingPty) {
    RunShellCommandSequence(ExecDefaultShellOnSlave, "default shell");
}

TEST(CubeSandboxPtyExecChain, ShellRepeatedlyRunsLsThenUnameThroughControllingPty) {
    RunRepeatedShellCommandSequence(ExecDefaultShellOnSlave, "default shell");
}

TEST(CubeSandboxPtyExecChain, BusyBoxAshRunsLsThenUnameThroughControllingPty) {
    if (access("/bin/busybox", X_OK) != 0) {
        GTEST_SKIP() << "/bin/busybox is not available on this host";
    }
    RunShellCommandSequence(ExecBusyBoxShellOnSlave, "busybox ash");
}

TEST(CubeSandboxPtyExecChain, MockShimForwardsClientInputAndCollectsExecOutput) {
    RunCubeShimLikeExecChain(ExecDefaultShellOnSlave, "default shell");
}

TEST(CubeSandboxPtyExecChain, MockShimConcurrentForwardersPublishExecProgress) {
    RunCubeShimLikeConcurrentForwarders(ExecDefaultShellOnSlave, "default shell");
}

TEST(CubeSandboxPtyExecChain, MockShimByteStreamInputCommandsReachShell) {
    RunCubeShimLikeByteStreamInput(ExecDefaultShellOnSlave, "default shell");
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
