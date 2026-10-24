#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <sys/ptrace.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

namespace {

void Check(bool value, int code) {
    if (!value) _exit(code);
}

void Reap(pid_t pid) {
    int status;
    Check(waitpid(pid, &status, 0) == pid && WIFEXITED(status) &&
              WEXITSTATUS(status) == 0, 70);
}

void SleepForever() {
    for (;;) pause();
}

// Each scenario runs in a disposable supervisor. A pipe held by every
// descendant must reach EOF after namespace init is reaped: observing only
// init's exit would miss leaked grandchildren and premature reaper removal.
void InNamespace(void (*scenario)(), bool outside_child = false,
                 bool outside_forks = false) {
    fflush(nullptr);
    pid_t supervisor = fork();
    ASSERT_GE(supervisor, 0);
    if (supervisor == 0) {
        Check(setsid() >= 0, 90);
        Check(unshare(CLONE_NEWUSER | CLONE_NEWPID) == 0, 91);
        int lifetime[2];
        Check(pipe(lifetime) == 0, 92);
        int gate[2] = {-1, -1};
        if (outside_child || outside_forks) Check(pipe(gate) == 0, 96);
        pid_t init = fork();
        Check(init >= 0, 93);
        if (init == 0) {
            close(lifetime[0]);
            if (outside_child || outside_forks) {
                close(gate[1]);
                char byte;
                Check(read(gate[0], &byte, 1) == 1, 97);
                close(gate[0]);
            }
            scenario();
            _exit(0);
        }
        close(lifetime[1]);
        if (outside_child) {
            close(gate[0]);
            pid_t sibling = fork();
            Check(sibling >= 0, 80);
            if (sibling == 0) _exit(0);
            siginfo_t info{};
            Check(waitid(P_PID, sibling, &info, WEXITED | WNOWAIT) == 0, 81);
            Check(write(gate[1], "r", 1) == 1, 82);
            close(gate[1]);
            usleep(100000);
            // This zombie belongs to the outer supervisor. Namespace init
            // must remain alive until the supervisor releases its PID.
            Check(waitpid(init, nullptr, WNOHANG) == 0, 83);
            Reap(sibling);
        }
        if (outside_forks) {
            close(gate[0]);
            Check(write(gate[1], "r", 1) == 1, 85);
            close(gate[1]);
            // This supervisor lives outside the namespace it forks into.
            // Its final publication cannot rely on a killed parent's flags.
            for (int i = 0; i < 64; ++i) {
                pid_t child = fork();
                if (child == 0) _exit(0);
                if (child < 0) {
                    Check(errno == ENOMEM, 86);
                    break;
                }
                Check(waitpid(child, nullptr, 0) == child, 87);
            }
        }
        Reap(init);
        errno = 0;
        Check(fork() == -1 && errno == ENOMEM, 84);
        pollfd event{lifetime[0], POLLIN | POLLHUP, 0};
        Check(poll(&event, 1, 3000) == 1, 94);
        char byte;
        Check(read(lifetime[0], &byte, 1) == 0, 95);
        _exit(0);
    }
    int status = 0;
    pid_t done = 0;
    for (int i = 0; i < 200; ++i) {
        done = waitpid(supervisor, &status, WNOHANG);
        if (done != 0) break;
        usleep(100000);
    }
    kill(-supervisor, SIGKILL);
    if (done == 0) {
        kill(supervisor, SIGKILL);
        waitpid(supervisor, &status, 0);
        FAIL() << "PID namespace shutdown exceeded 20 seconds";
    }
    ASSERT_EQ(done, supervisor);
    ASSERT_TRUE(WIFEXITED(status)) << status;
    EXPECT_EQ(WEXITSTATUS(status), 0);
}

void ForkDescendantTree() {
    int ready[2];
    Check(pipe(ready) == 0, 10);
    pid_t child = fork();
    Check(child >= 0, 11);
    if (child == 0) {
        close(ready[0]);
        pid_t grandchild = fork();
        Check(grandchild >= 0, 12);
        if (grandchild == 0) {
            Check(write(ready[1], "r", 1) == 1, 13);
            SleepForever();
        }
        SleepForever();
    }
    close(ready[1]);
    char byte;
    Check(read(ready[0], &byte, 1) == 1, 14);
    close(ready[0]);
}

void ExistingZombieAndDescendants() {
    pid_t zombie = fork();
    Check(zombie >= 0, 15);
    if (zombie == 0) _exit(0);
    siginfo_t info{};
    Check(waitid(P_PID, zombie, &info, WEXITED | WNOWAIT) == 0, 16);
    ForkDescendantTree();
}

void* LastInitThread(void*) {
    // The leader must have entered SYS_exit before this fork. Namespace PID
    // allocation and reaper ownership remain usable while this thread lives.
    usleep(100000);
    pid_t child = fork();
    Check(child >= 0, 20);
    if (child == 0) _exit(0);
    Reap(child);
    ForkDescendantTree();
    syscall(SYS_exit, 0);
    return nullptr;
}

void LeaderExitsFirst() {
    pthread_t thread;
    Check(pthread_create(&thread, nullptr, LastInitThread, nullptr) == 0, 21);
    syscall(SYS_exit, 0);
    _exit(22);
}

void NestedNamespace() {
    int ready[2];
    Check(pipe(ready) == 0, 30);
    Check(unshare(CLONE_NEWPID) == 0, 31);
    pid_t nested = fork();
    Check(nested >= 0, 32);
    if (nested == 0) {
        close(ready[0]);
        Check(getpid() == 1, 33);
        ForkDescendantTree();
        Check(write(ready[1], "r", 1) == 1, 34);
        SleepForever();
    }
    close(ready[1]);
    char byte;
    Check(read(ready[0], &byte, 1) == 1, 35);
    close(ready[0]);
}

void InitIsTracer() {
    pid_t child = fork();
    Check(child >= 0, 40);
    if (child == 0) {
        Check(ptrace(PTRACE_TRACEME, 0, nullptr, nullptr) == 0, 41);
        raise(SIGSTOP);
        SleepForever();
    }
    int status;
    Check(waitpid(child, &status, WUNTRACED) == child && WIFSTOPPED(status), 42);
}

void ForkRacesShutdown() {
    int ready[2];
    Check(pipe(ready) == 0, 50);
    pid_t child = fork();
    Check(child >= 0, 51);
    if (child == 0) {
        close(ready[0]);
        Check(write(ready[1], "r", 1) == 1, 52);
        for (;;) {
            pid_t grandchild = fork();
            if (grandchild == 0) _exit(0);
            if (grandchild > 0) waitpid(grandchild, nullptr, 0);
            if (grandchild < 0 && errno == ENOMEM) SleepForever();
        }
    }
    close(ready[1]);
    char byte;
    Check(read(ready[0], &byte, 1) == 1, 53);
    close(ready[0]);
    usleep(20000);
}

void ReparentZombie(bool no_cldwait) {
    struct sigaction action{};
    action.sa_handler = no_cldwait ? SIG_DFL : SIG_IGN;
    action.sa_flags = no_cldwait ? SA_NOCLDWAIT : 0;
    Check(sigaction(SIGCHLD, &action, nullptr) == 0, 60);
    int report[2];
    Check(pipe(report) == 0, 61);
    pid_t parent = fork();
    Check(parent >= 0, 62);
    if (parent == 0) {
        close(report[0]);
        action.sa_handler = SIG_DFL;
        action.sa_flags = 0;
        Check(sigaction(SIGCHLD, &action, nullptr) == 0, 63);
        pid_t zombie = fork();
        Check(zombie >= 0, 64);
        if (zombie == 0) _exit(0);
        siginfo_t info{};
        Check(waitid(P_PID, zombie, &info, WEXITED | WNOWAIT) == 0, 65);
        Check(write(report[1], &zombie, sizeof(zombie)) == sizeof(zombie), 66);
        _exit(0);
    }
    close(report[1]);
    pid_t zombie;
    Check(read(report[0], &zombie, sizeof(zombie)) == sizeof(zombie), 67);
    close(report[0]);
    for (int i = 0; i < 100; ++i) {
        if (kill(zombie, 0) == -1 && errno == ESRCH) return;
        usleep(10000);
    }
    _exit(68);
}

}  // namespace

TEST(PidNamespaceExit, KillsDescendantsAndReapsExistingZombies) {
    InNamespace(ExistingZombieAndDescendants);
}

TEST(PidNamespaceExit, LeaderExitPreservesNamespaceUntilLastThread) {
    InNamespace(LeaderExitsFirst);
}

TEST(PidNamespaceExit, KillsNestedNamespaceInitAndDescendants) {
    InNamespace(NestedNamespace);
}

TEST(PidNamespaceExit, DetachesInitTraceesBeforeWaiting) {
    InNamespace(InitIsTracer);
}

TEST(PidNamespaceExit, ConcurrentForkCannotEscapeShutdown) {
    for (int i = 0; i < 8; ++i) InNamespace(ForkRacesShutdown);
}

TEST(PidNamespaceExit, WaitsForZombieOwnedByOuterParent) {
    InNamespace([] {}, true);
}

TEST(PidNamespaceExit, ReparentedZombieHonorsIgnoredSigchld) {
    InNamespace([] { ReparentZombie(false); });
}

TEST(PidNamespaceExit, ReparentedZombieHonorsNoCldwait) {
    InNamespace([] { ReparentZombie(true); });
}

TEST(PidNamespaceExit, OuterParentForkCannotPublishIntoDyingNamespace) {
    for (int i = 0; i < 8; ++i) InNamespace([] {}, false, true);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
