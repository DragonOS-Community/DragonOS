#include <gtest/gtest.h>
#include <cerrno>
#include <csignal>
#include <fcntl.h>
#include <poll.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

namespace {
struct Owner { int type; pid_t pid; };
struct Fd {
    int value = -1;
    ~Fd() { if (value >= 0) close(value); }
};
struct Child {
    pid_t pid = -1;
    ~Child() { if (pid > 0) { kill(pid, SIGKILL); waitpid(pid, nullptr, 0); } }
};

TEST(FileOwnerNotifications, ExtendedOwnerTypesAndValidation) {
    Fd socket {::socket(AF_INET, SOCK_STREAM, 0)};
    ASSERT_GE(socket.value, 0);
    for (int kind : {F_OWNER_TID, F_OWNER_PID, F_OWNER_PGRP}) {
        Owner expected {kind, kind == F_OWNER_PGRP ? getpgrp() : getpid()};
        ASSERT_EQ(0, fcntl(socket.value, F_SETOWN_EX, &expected)) << strerror(errno);
        Owner output {-1, -1};
        ASSERT_EQ(0, fcntl(socket.value, F_GETOWN_EX, &output)) << strerror(errno);
        EXPECT_EQ(expected.type, output.type);
        EXPECT_EQ(expected.pid, output.pid);
    }
    Owner invalid {99, getpid()};
    errno = 0;
    EXPECT_EQ(-1, fcntl(socket.value, F_SETOWN_EX, &invalid));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, fcntl(socket.value, F_GETOWN_EX, nullptr));
    EXPECT_EQ(EFAULT, errno);
    errno = 0;
    EXPECT_EQ(-1, fcntl(socket.value, F_SETOWN, static_cast<int>(0x80000000U)));
    EXPECT_EQ(EINVAL, errno);
}

TEST(FileOwnerNotifications, LegacyNegativeOwnerNamesAProcessGroup) {
    int channel[2];
    ASSERT_EQ(0, pipe(channel));
    Fd input {channel[0]}, output {channel[1]};
    Child child {fork()};
    ASSERT_GE(child.pid, 0);
    if (child.pid == 0) {
        char value;
        _exit(read(input.value, &value, 1) == 1 ? 0 : 2);
    }
    ASSERT_EQ(0, setpgid(child.pid, child.pid));
    Fd socket {::socket(AF_INET, SOCK_STREAM, 0)};
    ASSERT_GE(socket.value, 0);
    ASSERT_EQ(0, fcntl(socket.value, F_SETOWN, -child.pid));
    EXPECT_EQ(-child.pid, fcntl(socket.value, F_GETOWN));
    Owner owner {};
    ASSERT_EQ(0, fcntl(socket.value, F_GETOWN_EX, &owner));
    EXPECT_EQ(F_OWNER_PGRP, owner.type);
    EXPECT_EQ(child.pid, owner.pid);
    ASSERT_EQ(1, write(output.value, "x", 1));
    int status = 0;
    ASSERT_EQ(child.pid, waitpid(child.pid, &status, 0));
    child.pid = -1;
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, fcntl(socket.value, F_GETOWN));
    ASSERT_EQ(0, fcntl(socket.value, F_GETOWN_EX, &owner));
    EXPECT_EQ(0, owner.pid);
    EXPECT_EQ(F_OWNER_PGRP, owner.type);
}

TEST(FileOwnerNotifications, DupSharesOwnerAndSetterType) {
    Fd first {::socket(AF_INET, SOCK_STREAM, 0)};
    ASSERT_GE(first.value, 0);
    Fd second {dup(first.value)};
    ASSERT_GE(second.value, 0);
    Owner expected {F_OWNER_TID, getpid()};
    ASSERT_EQ(0, fcntl(first.value, F_SETOWN_EX, &expected));
    Owner output {};
    ASSERT_EQ(0, fcntl(second.value, F_GETOWN_EX, &output));
    EXPECT_EQ(expected.type, output.type);
    EXPECT_EQ(expected.pid, output.pid);
    ASSERT_EQ(0, fcntl(second.value, F_SETOWN, 0));
    EXPECT_EQ(0, fcntl(first.value, F_GETOWN));
}

TEST(FileOwnerNotifications, SigioUsesSetterIdentityRatherThanTriggeringTask) {
    ASSERT_EQ(0, geteuid()) << "isolated credential regression requires root";
    int data[2], report[2];
    ASSERT_EQ(0, pipe(data));
    ASSERT_EQ(0, pipe(report));
    Fd read_data {data[0]}, write_data {data[1]};
    Fd read_report {report[0]}, write_report {report[1]};
    Child target {fork()};
    ASSERT_GE(target.pid, 0);
    if (target.pid == 0) {
        sigset_t blocked;
        sigemptyset(&blocked);
        sigaddset(&blocked, SIGIO);
        if (sigprocmask(SIG_BLOCK, &blocked, nullptr) || setuid(1002)) _exit(2);
        if (write(write_report.value, "r", 1) != 1) _exit(3);
        timespec deadline {0, 500000000};
        errno = 0;
        int result = sigtimedwait(&blocked, nullptr, &deadline);
        int error = errno;
        int values[] = {result, error};
        if (write(write_report.value, values, sizeof(values)) != sizeof(values)) _exit(4);
        _exit(0);
    }
    pollfd startup {read_report.value, POLLIN, 0};
    ASSERT_EQ(1, poll(&startup, 1, 3000));
    char ready = 0;
    ASSERT_EQ(1, read(read_report.value, &ready, 1));
    Child setter {fork()};
    ASSERT_GE(setter.pid, 0);
    if (setter.pid == 0) {
        if (setuid(1001)) _exit(2);
        if (fcntl(read_data.value, F_SETOWN, target.pid)) _exit(3);
        int flags = fcntl(read_data.value, F_GETFL);
        if (flags < 0 || fcntl(read_data.value, F_SETFL, flags | O_ASYNC)) _exit(4);
        _exit(0);
    }
    int status = 0;
    ASSERT_EQ(setter.pid, waitpid(setter.pid, &status, 0));
    setter.pid = -1;
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(0, WEXITSTATUS(status));
    ASSERT_EQ(1, write(write_data.value, "x", 1));
    pollfd result {read_report.value, POLLIN, 0};
    ASSERT_EQ(1, poll(&result, 1, 2000));
    int values[2] {};
    ASSERT_EQ(sizeof(values), read(read_report.value, values, sizeof(values)));
    EXPECT_EQ(-1, values[0]) << "unrelated setter must not authorize SIGIO delivery";
    EXPECT_EQ(EAGAIN, values[1]);
    ASSERT_EQ(target.pid, waitpid(target.pid, &status, 0));
    target.pid = -1;
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(FileOwnerNotifications, ProcessGroupOwnerDeliversKernelSigio) {
    int data[2], report[2];
    ASSERT_EQ(0, pipe(data));
    ASSERT_EQ(0, pipe(report));
    Fd read_data {data[0]}, write_data {data[1]};
    Fd read_report {report[0]}, write_report {report[1]};
    Child target {fork()};
    ASSERT_GE(target.pid, 0);
    if (target.pid == 0) {
        sigset_t blocked;
        sigemptyset(&blocked);
        sigaddset(&blocked, SIGIO);
        if (setpgid(0, 0) || sigprocmask(SIG_BLOCK, &blocked, nullptr)) _exit(2);
        if (write(write_report.value, "r", 1) != 1) _exit(3);
        timespec timeout {2, 0};
        siginfo_t info {};
        int values[] = {sigtimedwait(&blocked, &info, &timeout), info.si_code};
        if (write(write_report.value, values, sizeof(values)) != sizeof(values)) _exit(4);
        _exit(0);
    }
    pollfd ready {read_report.value, POLLIN, 0};
    ASSERT_EQ(1, poll(&ready, 1, 3000));
    char value = 0;
    ASSERT_EQ(1, read(read_report.value, &value, 1));
    ASSERT_EQ(0, fcntl(read_data.value, F_SETOWN, -target.pid));
    int flags = fcntl(read_data.value, F_GETFL);
    ASSERT_GE(flags, 0);
    ASSERT_EQ(0, fcntl(read_data.value, F_SETFL, flags | O_ASYNC));
    ASSERT_EQ(1, write(write_data.value, "x", 1));
    ASSERT_EQ(1, poll(&ready, 1, 3000));
    int values[2] {};
    ASSERT_EQ(sizeof(values), read(read_report.value, values, sizeof(values)));
    EXPECT_EQ(SIGIO, values[0]);
    EXPECT_EQ(SI_KERNEL, values[1]);
    int status = 0;
    ASSERT_EQ(target.pid, waitpid(target.pid, &status, 0));
    target.pid = -1;
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}

} // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
