#include <gtest/gtest.h>

#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

namespace {

void* fork_worker(void*) {
    pid_t pid = fork();
    if (pid == 0) {
        _exit(0);
    }
    if (pid > 0) {
        waitpid(pid, nullptr, 0);
    }
    return nullptr;
}

}  // namespace

TEST(PidNamespaceFork, DeadPidNamespaceForChildrenDoesNotPanic) {
    if (unshare(CLONE_NEWPID) != 0) {
        GTEST_SKIP() << "unshare(CLONE_NEWPID) failed: " << strerror(errno);
    }

    pid_t first = fork();
    ASSERT_GE(first, 0) << strerror(errno);
    if (first == 0) {
        _exit(0);
    }
    ASSERT_EQ(waitpid(first, nullptr, 0), first) << strerror(errno);

    errno = 0;
    pid_t second = fork();
    if (second == 0) {
        _exit(0);
    }
    ASSERT_EQ(second, -1) << "fork unexpectedly succeeded in a dead child PID namespace";
    EXPECT_EQ(errno, ENOMEM) << "unexpected fork errno: " << strerror(errno);

    pthread_t thread;
    int rc = pthread_create(&thread, nullptr, fork_worker, nullptr);
    ASSERT_NE(rc, 0) << "pthread_create unexpectedly succeeded in a dead child PID namespace";
    EXPECT_TRUE(rc == ENOMEM || rc == EAGAIN)
        << "unexpected pthread_create error: " << strerror(rc);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
