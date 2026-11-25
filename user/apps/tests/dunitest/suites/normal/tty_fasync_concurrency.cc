#include <gtest/gtest.h>

#include <fcntl.h>
#include <pthread.h>
#include <pty.h>
#include <unistd.h>

namespace {

constexpr int kThreads = 8;
constexpr int kRounds = 20000;

void* ToggleAsync(void* arg) {
    int fd = *static_cast<int*>(arg);
    for (int round = 0; round < kRounds; round++) {
        EXPECT_EQ(0, fcntl(fd, F_SETFL, O_ASYNC));
        EXPECT_EQ(0, fcntl(fd, F_SETFL, 0));
    }
    return nullptr;
}

TEST(TtyFasyncConcurrency, AsyncTogglesOnOneTtyFromManyThreads) {
    int master = -1;
    int slave = -1;
    char name[128] = {};
    ASSERT_EQ(0, openpty(&master, &slave, name, nullptr, nullptr));

    int fds[kThreads];
    pthread_t threads[kThreads];
    for (int i = 0; i < kThreads; i++) {
        fds[i] = open(name, O_RDWR | O_NOCTTY);
        ASSERT_GE(fds[i], 0);
    }
    for (int i = 0; i < kThreads; i++) {
        ASSERT_EQ(0, pthread_create(&threads[i], nullptr, ToggleAsync, &fds[i]));
    }
    for (int i = 0; i < kThreads; i++) {
        pthread_join(threads[i], nullptr);
        close(fds[i]);
    }
    close(slave);
    close(master);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
