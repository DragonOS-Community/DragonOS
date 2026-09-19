#include <gtest/gtest.h>

#include <fcntl.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#include <algorithm>
#include <atomic>
#include <cerrno>
#include <cstring>
#include <thread>
#include <vector>

namespace {

constexpr int kSentFds = 3;
constexpr char kPayload[] = "rights";

// Each scenario runs in a child: a failing kernel must not leak received fds or
// a lowered RLIMIT_NOFILE into another test (including on ASSERT_* failures).
template <typename F>
void InChild(F scenario) {
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        alarm(10);
        scenario();
        _exit(testing::Test::HasFailure() ? 1 : 0);
    }
    int status = 0;
    pid_t result;
    do {
        result = waitpid(child, &status, 0);
    } while (result < 0 && errno == EINTR);
    ASSERT_EQ(result, child);
    ASSERT_TRUE(WIFEXITED(status)) << "child status=" << status;
    EXPECT_EQ(WEXITSTATUS(status), 0);
}

class UnixScmRights : public testing::TestWithParam<int> {};

// fault_after >= 0 places that many complete fd slots before PROT_NONE.
// available >= 0 exhausts the fd table, then frees precisely that many slots.
void ReceiveRights(int type, size_t capacity, int expected, bool cloexec,
                   int available = -1, int fault_after = -1, bool header_fault = false,
                   bool passcred = false) {
    int sockets[2];
    ASSERT_EQ(socketpair(AF_UNIX, type, 0, sockets), 0);
    if (passcred) {
        const int enabled = 1;
        ASSERT_EQ(setsockopt(sockets[1], SOL_SOCKET, SO_PASSCRED, &enabled, sizeof(enabled)), 0);
    }
    const int source = open("/dev/null", O_RDONLY | O_CLOEXEC);
    ASSERT_GE(source, 0);
    struct stat source_stat {};
    ASSERT_EQ(fstat(source, &source_stat), 0);

    alignas(cmsghdr) char sent_control[CMSG_SPACE(kSentFds * sizeof(int))] = {};
    iovec sent_iov {const_cast<char*>(kPayload), sizeof(kPayload)};
    msghdr sent {};
    sent.msg_iov = &sent_iov;
    sent.msg_iovlen = 1;
    sent.msg_control = sent_control;
    sent.msg_controllen = sizeof(sent_control);
    cmsghdr* header = CMSG_FIRSTHDR(&sent);
    header->cmsg_level = SOL_SOCKET;
    header->cmsg_type = SCM_RIGHTS;
    header->cmsg_len = CMSG_LEN(kSentFds * sizeof(int));
    const int sent_fds[kSentFds] = {source, source, source};
    std::memcpy(CMSG_DATA(header), sent_fds, sizeof(sent_fds));
    ASSERT_EQ(sendmsg(sockets[0], &sent, 0), static_cast<ssize_t>(sizeof(kPayload)));

    // Filling after send avoids testing sender-side in-flight limits instead.
    if (available >= 0) {
        rlimit limit {};
        ASSERT_EQ(getrlimit(RLIMIT_NOFILE, &limit), 0);
        limit.rlim_cur = std::min<rlim_t>(limit.rlim_cur, 128);
        ASSERT_EQ(setrlimit(RLIMIT_NOFILE, &limit), 0);
        std::vector<int> filler;
        for (;;) {
            int fd = dup(source);
            if (fd < 0) {
                ASSERT_EQ(errno, EMFILE);
                break;
            }
            filler.push_back(fd);
        }
        ASSERT_GE(filler.size(), static_cast<size_t>(available));
        for (int i = 0; i < available; ++i) {
            ASSERT_EQ(close(filler.back()), 0);
            filler.pop_back();
        }
    }

    const long page = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page, 0);
    void* mapping = mmap(nullptr, page * 2, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(mapping, MAP_FAILED);
    char* control = static_cast<char*>(mapping);
    if (fault_after >= 0) {
        ASSERT_EQ(mprotect(control + page, page, PROT_NONE), 0);
        control += page - CMSG_LEN(fault_after * sizeof(int));
    } else if (header_fault) {
        ASSERT_EQ(mprotect(control, page, PROT_READ), 0);
        control += page - CMSG_LEN(0);
    }

    // The lowest free slot before reception lets us detect leaked unpublished
    // descriptors, including a reservation whose fd copyout failed.
    int first_free = -1;
    if (available != 0) {
        first_free = dup(source);
        ASSERT_GE(first_free, 0);
        ASSERT_EQ(close(first_free), 0);
    }
    char payload[sizeof(kPayload)] = {};
    iovec iov {payload, sizeof(payload)};
    msghdr msg {};
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    msg.msg_control = capacity ? control : nullptr;
    msg.msg_controllen = capacity;
    ASSERT_EQ(recvmsg(sockets[1], &msg, MSG_DONTWAIT | (cloexec ? MSG_CMSG_CLOEXEC : 0)),
              static_cast<ssize_t>(sizeof(kPayload))) << std::strerror(errno);
    EXPECT_EQ(std::memcmp(payload, kPayload, sizeof(payload)), 0);
    EXPECT_EQ((msg.msg_flags & MSG_CTRUNC) != 0, expected < kSentFds);
    const size_t credentials_used = passcred ? CMSG_SPACE(sizeof(ucred)) : 0;
    const size_t rights_used = expected && !header_fault
                            ? std::min<size_t>(CMSG_SPACE(expected * sizeof(int)),
                                               capacity - credentials_used) : 0;
    const size_t used = rights_used + credentials_used;
    ASSERT_EQ(msg.msg_controllen, used);

    if (passcred) {
        cmsghdr credentials_header {};
        std::memcpy(&credentials_header, control, sizeof(credentials_header));
        ASSERT_EQ(credentials_header.cmsg_level, SOL_SOCKET);
        ASSERT_EQ(credentials_header.cmsg_type, SCM_CREDENTIALS);
        ASSERT_EQ(credentials_header.cmsg_len, CMSG_LEN(sizeof(ucred)));
        ucred credentials {};
        std::memcpy(&credentials, control + CMSG_LEN(0), sizeof(credentials));
        EXPECT_EQ(credentials.pid, getpid());
        EXPECT_EQ(credentials.uid, getuid());
        EXPECT_EQ(credentials.gid, getgid());
        control += CMSG_SPACE(sizeof(ucred));
    }

    std::vector<int> received;
    if (expected) {
        // memcpy permits the deliberately unaligned cmsg in odd-prefix cases.
        if (!header_fault) {
            cmsghdr result {};
            std::memcpy(&result, control, sizeof(result));
            EXPECT_EQ(result.cmsg_level, SOL_SOCKET);
            EXPECT_EQ(result.cmsg_type, SCM_RIGHTS);
            ASSERT_EQ(result.cmsg_len, CMSG_LEN(expected * sizeof(int)));
        }
        for (int i = 0; i < expected; ++i) {
            int fd;
            std::memcpy(&fd, control + CMSG_LEN(0) + i * sizeof(fd), sizeof(fd));
            ASSERT_GE(fd, 0);
            received.push_back(fd);
            EXPECT_EQ(fd, first_free + i);
            ASSERT_GE(fcntl(fd, F_GETFD), 0);
            EXPECT_EQ((fcntl(fd, F_GETFD) & FD_CLOEXEC) != 0, cloexec);
            struct stat st {};
            ASSERT_EQ(fstat(fd, &st), 0);
            EXPECT_EQ(st.st_dev, source_stat.st_dev);
            EXPECT_EQ(st.st_ino, source_stat.st_ino);
        }
    }
    int next = dup(source);
    if (available >= 0 && available == expected) {
        EXPECT_EQ(next, -1);
        EXPECT_EQ(errno, EMFILE);
    } else {
        ASSERT_GE(next, 0);
        EXPECT_EQ(next, first_free + expected);
        ASSERT_EQ(close(next), 0);
    }
    for (int fd : received) {
        ASSERT_EQ(close(fd), 0);
    }
    if (available != 0) {
        next = dup(source);
        ASSERT_GE(next, 0);
        EXPECT_EQ(next, first_free);
        ASSERT_EQ(close(next), 0);
    }
    // recvmsg consumed the payload even when no descriptor could be delivered.
    EXPECT_EQ(recv(sockets[1], payload, sizeof(payload), MSG_DONTWAIT), -1);
    EXPECT_EQ(errno, EAGAIN);
    ASSERT_EQ(munmap(mapping, page * 2), 0);
    close(source);
    close(sockets[0]);
    close(sockets[1]);
}

TEST_P(UnixScmRights, CompleteAndCloexec) {
    for (bool cloexec : {false, true}) {
        InChild([&] { ReceiveRights(GetParam(), CMSG_SPACE(kSentFds * sizeof(int)), kSentFds, cloexec); });
    }
}

TEST_P(UnixScmRights, ControlCapacity) {
    // Exact lengths exercise missing final padding; CMSG_SPACE(sizeof(int))
    // actually fits two descriptors on 64-bit platforms.
    for (int count = 0; count <= kSentFds; ++count) {
        InChild([&] { ReceiveRights(GetParam(), CMSG_LEN(count * sizeof(int)), count, false); });
    }
    InChild([&] { ReceiveRights(GetParam(), 0, 0, false); });
    InChild([&] { ReceiveRights(GetParam(), CMSG_LEN(0) - 1, 0, false); });
}

TEST_P(UnixScmRights, FileLimitKeepsDeliveredPrefix) {
    for (int available = 0; available < kSentFds; ++available) {
        InChild([&] { ReceiveRights(GetParam(), CMSG_SPACE(kSentFds * sizeof(int)),
                                   available, true, available); });
    }
}

TEST_P(UnixScmRights, FaultKeepsDeliveredPrefixAndReleasesFailedSlot) {
    for (int prefix = 0; prefix < kSentFds; ++prefix) {
        InChild([&] { ReceiveRights(GetParam(), CMSG_SPACE(kSentFds * sizeof(int)),
                                   prefix, true, -1, prefix); });
    }
}

TEST_P(UnixScmRights, HeaderFaultDoesNotUndoPublishedDescriptors) {
    InChild([&] { ReceiveRights(GetParam(), CMSG_SPACE(kSentFds * sizeof(int)),
                               kSentFds, false, -1, -1, true); });
}

TEST_P(UnixScmRights, CredentialsPrecedePartialRights) {
    InChild([&] { ReceiveRights(GetParam(), CMSG_SPACE(sizeof(ucred)) + CMSG_LEN(sizeof(int)),
                               1, false, -1, -1, false, true); });
}

TEST_P(UnixScmRights, ConcurrentCloseAndReuseDuringFault) {
    // Supplemental stress, not a deterministic race reproducer. Once an fd is
    // visible, another thread may close/reuse it. Ancillary failure must never
    // roll back that thread's replacement file by the old numeric fd alone.
    for (int iteration = 0; iteration < 20; ++iteration) {
        InChild([&] {
            int sockets[2];
            ASSERT_EQ(socketpair(AF_UNIX, GetParam(), 0, sockets), 0);
            const int source = open("/dev/null", O_RDONLY);
            const int replacement = open("/dev/zero", O_RDONLY);
            ASSERT_GE(source, 0);
            ASSERT_GE(replacement, 0);
            struct stat replacement_stat {};
            ASSERT_EQ(fstat(replacement, &replacement_stat), 0);
            constexpr int count = 200;
            alignas(cmsghdr) char sent_control[CMSG_SPACE(count * sizeof(int))] = {};
            char byte = 'x';
            iovec iov {&byte, 1};
            msghdr msg {};
            msg.msg_iov = &iov;
            msg.msg_iovlen = 1;
            msg.msg_control = sent_control;
            msg.msg_controllen = sizeof(sent_control);
            auto* hdr = CMSG_FIRSTHDR(&msg);
            hdr->cmsg_level = SOL_SOCKET;
            hdr->cmsg_type = SCM_RIGHTS;
            hdr->cmsg_len = CMSG_LEN(count * sizeof(int));
            for (int i = 0; i < count; ++i) {
                std::memcpy(CMSG_DATA(hdr) + i * sizeof(int), &source, sizeof(source));
            }
            ASSERT_EQ(sendmsg(sockets[0], &msg, 0), 1);
            const long page = sysconf(_SC_PAGESIZE);
            ASSERT_GT(page, static_cast<long>(CMSG_SPACE(count * sizeof(int))));
            void* area = mmap(nullptr, page * 2, PROT_READ | PROT_WRITE,
                              MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
            ASSERT_NE(area, MAP_FAILED);
            ASSERT_EQ(mprotect(static_cast<char*>(area) + page, page, PROT_NONE), 0);
            msg.msg_control = static_cast<char*>(area) + page - CMSG_LEN((count - 1) * sizeof(int));
            const int target = dup(source);
            ASSERT_GE(target, 0);
            ASSERT_EQ(close(target), 0);
            std::atomic<bool> ready {false};
            std::atomic<bool> finished {false};
            bool reused = false;
            std::thread closer([&] {
                ready.store(true, std::memory_order_release);
                while (!finished.load(std::memory_order_acquire)) {
                    if (fcntl(target, F_GETFD) >= 0) {
                        close(target);
                        // A concurrent reservation may return EBUSY; only a
                        // successful replacement establishes the invariant.
                        reused = dup2(replacement, target) == target;
                        break;
                    }
                    std::this_thread::yield();
                }
            });
            while (!ready.load(std::memory_order_acquire)) {
                std::this_thread::yield();
            }
            const ssize_t result = recvmsg(sockets[1], &msg, MSG_DONTWAIT);
            finished.store(true, std::memory_order_release);
            closer.join();
            EXPECT_EQ(result, 1);
            if (reused) {
                struct stat actual {};
                ASSERT_EQ(fstat(target, &actual), 0) << "replacement fd was rolled back";
                EXPECT_EQ(actual.st_dev, replacement_stat.st_dev);
                EXPECT_EQ(actual.st_ino, replacement_stat.st_ino);
                EXPECT_EQ(actual.st_rdev, replacement_stat.st_rdev);
            }
            ASSERT_EQ(munmap(area, page * 2), 0);
            // Child exit closes all received fds, including a target that may
            // legitimately have been reused while recvmsg was in progress.
        });
    }
}

INSTANTIATE_TEST_SUITE_P(SocketTypes, UnixScmRights,
                        testing::Values(SOCK_STREAM, SOCK_SEQPACKET, SOCK_DGRAM));

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
