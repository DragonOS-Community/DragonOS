#include <gtest/gtest.h>
#include <arpa/inet.h>
#include <fcntl.h>
#include <linux/tcp.h>
#include <poll.h>
#include <signal.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
#include <array>
#include <chrono>
#include <cstring>
#include <vector>
#include <future>
#include <thread>

namespace {
class Fd {
 public:
    explicit Fd(int fd = -1) : fd_(fd) {}
    ~Fd() { if (fd_ >= 0) close(fd_); }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int Get() const { return fd_; }
    int Release() { int result = fd_; fd_ = -1; return result; }
    void Set(int fd) { fd_ = fd; }
 private:
    int fd_;
};
class TcpUrgent : public testing::TestWithParam<int> {
 protected:
    void SetUp() override {
        Fd listener(socket(GetParam(), SOCK_STREAM, 0));
        ASSERT_GE(listener.Get(), 0);
        sockaddr_storage address{};
        socklen_t length;
        if (GetParam() == AF_INET) {
            auto* a = reinterpret_cast<sockaddr_in*>(&address);
            a->sin_family = AF_INET; a->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
            length = sizeof(*a);
        } else {
            auto* a = reinterpret_cast<sockaddr_in6*>(&address);
            a->sin6_family = AF_INET6; a->sin6_addr = in6addr_loopback;
            length = sizeof(*a);
        }
        ASSERT_EQ(0, bind(listener.Get(), reinterpret_cast<sockaddr*>(&address), length));
        ASSERT_EQ(0, getsockname(listener.Get(), reinterpret_cast<sockaddr*>(&address), &length));
        ASSERT_EQ(0, listen(listener.Get(), 2));
        writer_.Set(socket(GetParam(), SOCK_STREAM, 0));
        ASSERT_GE(writer_.Get(), 0);
        ASSERT_EQ(0, connect(writer_.Get(), reinterpret_cast<sockaddr*>(&address), length));
        reader_.Set(accept(listener.Get(), nullptr, nullptr));
        ASSERT_GE(reader_.Get(), 0);
        timeval timeout{2, 0};
        ASSERT_EQ(0, setsockopt(reader_.Get(), SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)));
        ASSERT_EQ(0, setsockopt(writer_.Get(), SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout)));
    }
    void Queue(const char* prefix = "abc", const char* urgent = "XY", const char* suffix = "def") {
        if (*prefix) ASSERT_EQ(static_cast<ssize_t>(strlen(prefix)), send(writer_.Get(), prefix, strlen(prefix), MSG_NOSIGNAL));
        ASSERT_EQ(static_cast<ssize_t>(strlen(urgent)), send(writer_.Get(), urgent, strlen(urgent), MSG_OOB | MSG_NOSIGNAL));
        if (*suffix) ASSERT_EQ(static_cast<ssize_t>(strlen(suffix)), send(writer_.Get(), suffix, strlen(suffix), MSG_NOSIGNAL));
        // PRI proves the urgent byte arrived, not the subsequently sent
        // ordinary suffix. Wait for all accepted data to be acknowledged:
        // Linux separates unsent bytes and unacked segments; DragonOS's
        // notsent field conservatively includes its entire transport queue.
        const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(2);
        for (;;) {
            tcp_info info{}; socklen_t length = sizeof(info);
            ASSERT_EQ(0, getsockopt(writer_.Get(), IPPROTO_TCP, TCP_INFO, &info, &length));
            ASSERT_GE(length, offsetof(tcp_info, tcpi_notsent_bytes) + sizeof(info.tcpi_notsent_bytes));
            if (info.tcpi_unacked == 0 && info.tcpi_notsent_bytes == 0) break;
            ASSERT_LT(std::chrono::steady_clock::now(), deadline) << "accepted suffix was not acknowledged";
            usleep(1000);
        }
        WaitPri();
    }
    void WaitPri() {
        pollfd ready{reader_.Get(), POLLPRI, 0};
        ASSERT_EQ(1, poll(&ready, 1, 2000));
        ASSERT_NE(0, ready.revents & POLLPRI);
    }
    int Mark() {
        int result = -1;
        EXPECT_EQ(0, ioctl(reader_.Get(), 0x8905 /* SIOCATMARK */, &result));
        return result;
    }
    int Inq() {
        int result = -1;
        EXPECT_EQ(0, ioctl(reader_.Get(), FIONREAD, &result));
        return result;
    }
    void Read(const char* expected, int flags = 0) {
        std::array<char, 64> bytes{};
        const ssize_t size = recv(reader_.Get(), bytes.data(), bytes.size(), flags);
        ASSERT_EQ(static_cast<ssize_t>(strlen(expected)), size) << errno;
        EXPECT_EQ(0, memcmp(bytes.data(), expected, strlen(expected)));
    }
    void NoOob() {
        char byte;
        errno = 0;
        EXPECT_EQ(-1, recv(reader_.Get(), &byte, 1, MSG_OOB | MSG_DONTWAIT));
        EXPECT_EQ(EINVAL, errno);
    }
    Fd reader_, writer_;
};

TEST_P(TcpUrgent, ScalarErrorQueueNeverConsumesOrdinaryData) {
    ASSERT_EQ(4, send(writer_.Get(), "data", 4, 0));
    pollfd ready{reader_.Get(), POLLIN, 0};
    ASSERT_EQ(1, poll(&ready, 1, 2000));
    for (int flags : std::array<int, 3>{MSG_ERRQUEUE, MSG_ERRQUEUE | MSG_OOB, MSG_ERRQUEUE | MSG_PEEK | MSG_TRUNC}) {
        char output[8]{};
        errno = 0;
        EXPECT_EQ(-1, recv(reader_.Get(), output, sizeof(output), flags));
        EXPECT_EQ(EAGAIN, errno);
        errno = 0;
        EXPECT_EQ(-1, recvfrom(reader_.Get(), nullptr, 0, flags, nullptr, nullptr));
        EXPECT_EQ(EAGAIN, errno);
    }
    Read("data");
}

TEST_P(TcpUrgent, EmptySpecialSourcesDoNotWaitAndRespectListenerOrder) {
    Fd fresh(socket(GetParam(), SOCK_STREAM, 0));
    Fd listener(socket(GetParam(), SOCK_STREAM, 0));
    ASSERT_GE(fresh.Get(), 0); ASSERT_GE(listener.Get(), 0);
    ASSERT_EQ(0, listen(listener.Get(), 1));
    for (int fd : {fresh.Get(), listener.Get(), reader_.Get()}) {
        timeval timeout{1, 0};
        ASSERT_EQ(0, setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)));
        for (int flags : std::array<int, 3>{MSG_ERRQUEUE, MSG_ERRQUEUE | MSG_OOB, MSG_OOB}) {
            char byte;
            const auto start = std::chrono::steady_clock::now();
            errno = 0;
            EXPECT_EQ(-1, recv(fd, &byte, 1, flags));
            EXPECT_EQ((flags & MSG_ERRQUEUE) ? EAGAIN : fd == listener.Get() ? ENOTCONN : EINVAL, errno);
            EXPECT_LT(std::chrono::steady_clock::now() - start, std::chrono::milliseconds(500));
        }
    }
}

TEST_P(TcpUrgent, BatchTimeoutDoesNotPrewaitTheOrdinaryQueueForOob) {
    Queue("", "!", "");
    char byte = 0;
    iovec vector{&byte, 1}; mmsghdr message{};
    message.msg_hdr.msg_iov = &vector; message.msg_hdr.msg_iovlen = 1;
    timespec timeout{1, 0};
    const auto start = std::chrono::steady_clock::now();
    ASSERT_EQ(1, recvmmsg(reader_.Get(), &message, 1, MSG_OOB, &timeout));
    EXPECT_LT(std::chrono::steady_clock::now() - start, std::chrono::milliseconds(500));
    EXPECT_EQ(1U, message.msg_len); EXPECT_EQ(MSG_OOB, message.msg_hdr.msg_flags);
    EXPECT_EQ('!', byte);
}

TEST_P(TcpUrgent, BatchEmptyErrorQueueIgnoresTimeoutAcrossInetSources) {
    Fd udp(socket(GetParam(), SOCK_DGRAM, 0));
    Fd raw(socket(GetParam(), SOCK_RAW, GetParam() == AF_INET ? int(IPPROTO_ICMP) : int(IPPROTO_ICMPV6)));
    ASSERT_GE(udp.Get(), 0); ASSERT_GE(raw.Get(), 0);
    for (int fd : {reader_.Get(), udp.Get(), raw.Get()}) {
        char byte = 0;
        iovec vector{&byte, 1}; mmsghdr message{};
        message.msg_hdr.msg_iov = &vector; message.msg_hdr.msg_iovlen = 1;
        timespec timeout{1, 0};
        const auto start = std::chrono::steady_clock::now();
        errno = 0;
        EXPECT_EQ(-1, recvmmsg(fd, &message, 1, MSG_ERRQUEUE, &timeout));
        EXPECT_EQ(EAGAIN, errno);
        EXPECT_LT(std::chrono::steady_clock::now() - start, std::chrono::milliseconds(500));
    }
}

TEST_P(TcpUrgent, PrefixUrgentSuffixAndOrdinaryMarkShareOneCursor) {
    Queue();
    EXPECT_EQ(4, Inq()); EXPECT_EQ(0, Mark());
    char urgent = 0;
    EXPECT_EQ(1, recv(reader_.Get(), &urgent, 1, MSG_OOB | MSG_PEEK));
    EXPECT_EQ('Y', urgent);
    Read("abcX");
    EXPECT_EQ(1, Mark()); EXPECT_EQ(0, Inq());
    EXPECT_EQ(1, recv(reader_.Get(), &urgent, 1, MSG_OOB));
    EXPECT_EQ('Y', urgent); EXPECT_EQ(1, Mark());
    NoOob();
    Read("def"); EXPECT_EQ(0, Mark());
}

TEST_P(TcpUrgent, OobPeekAndTruncDoNotCopyOrMoveNormalHead) {
    Queue("", "!", "x");
    char output = 'A';
    EXPECT_EQ(1, recv(reader_.Get(), &output, 1, MSG_OOB | MSG_PEEK | MSG_TRUNC));
    EXPECT_EQ('A', output); EXPECT_EQ(1, Mark());
    EXPECT_EQ(1, recv(reader_.Get(), &output, 1, MSG_OOB | MSG_TRUNC));
    EXPECT_EQ('A', output); EXPECT_EQ(1, Mark());
    NoOob(); Read("x");
}

TEST_P(TcpUrgent, RecvmsgOobUsesTheLogicalVectorAndReportsOnlyOob) {
    Queue("", "!", "x");
    char byte = 'A';
    iovec vectors[] = {{nullptr, 0}, {&byte, 1}};
    std::array<char, 64> control{};
    msghdr message{}; message.msg_iov = vectors; message.msg_iovlen = 2;
    message.msg_control = control.data(); message.msg_controllen = control.size();
    ASSERT_EQ(1, recvmsg(reader_.Get(), &message, MSG_OOB | MSG_PEEK));
    EXPECT_EQ('!', byte); EXPECT_EQ(MSG_OOB, message.msg_flags);
    EXPECT_EQ(0U, message.msg_namelen); EXPECT_EQ(0U, message.msg_controllen);
    Read("!", MSG_OOB); Read("x");
}

TEST_P(TcpUrgent, OobTcpInqReportsProtocolQueueNotFionreadPrefix) {
    int enabled = 1;
    ASSERT_EQ(0, setsockopt(reader_.Get(), IPPROTO_TCP, TCP_INQ, &enabled, sizeof(enabled)));
    Queue();
    char byte = 0; iovec vector{&byte, 1};
    std::array<char, 64> control{};
    msghdr message{}; message.msg_iov = &vector; message.msg_iovlen = 1;
    message.msg_control = control.data(); message.msg_controllen = control.size();
    ASSERT_EQ(1, recvmsg(reader_.Get(), &message, MSG_OOB | MSG_PEEK));
    EXPECT_EQ('Y', byte); EXPECT_EQ(MSG_OOB, message.msg_flags);
    const auto* header = CMSG_FIRSTHDR(&message);
    ASSERT_NE(nullptr, header); EXPECT_EQ(IPPROTO_TCP, header->cmsg_level);
    EXPECT_EQ(TCP_INQ, header->cmsg_type);
    int queued = 0; memcpy(&queued, CMSG_DATA(header), sizeof(queued));
    EXPECT_EQ(8, queued); EXPECT_EQ(4, Inq());
    message.msg_control = nullptr; message.msg_controllen = 0;
    ASSERT_EQ(1, recvmsg(reader_.Get(), &message, MSG_OOB));
    EXPECT_EQ(MSG_OOB | MSG_CTRUNC, message.msg_flags);
    enabled = 0;
    ASSERT_EQ(0, setsockopt(reader_.Get(), IPPROTO_TCP, TCP_INQ, &enabled, sizeof(enabled)));
    Read("abcX"); Read("def");
}

TEST_P(TcpUrgent, ZeroCapacityOobConsumesUnlessPeeking) {
    for (int flags : std::array<int, 3>{MSG_OOB, MSG_OOB | MSG_PEEK, MSG_OOB | MSG_TRUNC}) {
        Queue("", "!", "");
        iovec vector{nullptr, 0}; msghdr message{};
        message.msg_iov = &vector; message.msg_iovlen = 1;
        EXPECT_EQ(0, recvmsg(reader_.Get(), &message, flags));
        EXPECT_EQ(MSG_OOB | MSG_TRUNC, message.msg_flags);
        if (flags & MSG_PEEK) Read("!", MSG_OOB);
        else NoOob();
    }
}

TEST_P(TcpUrgent, OobFaultConsumesBeforeCopyButPeekFaultPreserves) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    void* inaccessible = mmap(nullptr, page, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, inaccessible);
    for (int flags : std::array<int, 2>{MSG_OOB | MSG_PEEK, MSG_OOB}) {
        Queue("", "!", "");
        errno = 0;
        EXPECT_EQ(-1, recv(reader_.Get(), inaccessible, 1, flags));
        EXPECT_EQ(EFAULT, errno);
        if (flags & MSG_PEEK) Read("!", MSG_OOB);
        else NoOob();
    }
    EXPECT_EQ(0, munmap(inaccessible, page));
}

TEST_P(TcpUrgent, InlineStillStopsAtMarkAndOobIsInvalid) {
    int enabled = 1;
    ASSERT_EQ(0, setsockopt(reader_.Get(), SOL_SOCKET, SO_OOBINLINE, &enabled, sizeof(enabled)));
    Queue();
    EXPECT_EQ(8, Inq()); NoOob();
    Read("abcX", MSG_WAITALL); EXPECT_EQ(1, Mark());
    Read("Ydef"); EXPECT_EQ(0, Mark());
}

TEST_P(TcpUrgent, InlineCanChangeWithoutLosingTheSavedByte) {
    Queue("", "!", "x");
    int enabled = 1;
    ASSERT_EQ(0, setsockopt(reader_.Get(), SOL_SOCKET, SO_OOBINLINE, &enabled, sizeof(enabled)));
    NoOob();
    enabled = 0;
    ASSERT_EQ(0, setsockopt(reader_.Get(), SOL_SOCKET, SO_OOBINLINE, &enabled, sizeof(enabled)));
    Read("!", MSG_OOB); Read("x");
}

TEST_P(TcpUrgent, OrdinaryPeekWaitallAndReadvStopAtTheSameMark) {
    Queue();
    Read("abcX", MSG_PEEK | MSG_WAITALL); EXPECT_EQ(0, Mark());
    Read("abcX", MSG_PEEK); EXPECT_EQ(0, Mark());
    std::array<char, 8> first{}, second{};
    iovec vectors[] = {{first.data(), 2}, {second.data(), second.size()}};
    EXPECT_EQ(4, readv(reader_.Get(), vectors, 2));
    EXPECT_EQ(0, memcmp(first.data(), "ab", 2));
    EXPECT_EQ(0, memcmp(second.data(), "cX", 2));
    EXPECT_EQ(1, Mark());
    Read("def", MSG_PEEK); EXPECT_EQ(1, Mark());
    Read("Y", MSG_OOB); Read("def");
}

TEST_P(TcpUrgent, OrdinaryTruncUsesTheSameBoundaryAndHole) {
    Queue();
    std::array<char, 32> bytes{}; bytes.fill('A');
    EXPECT_EQ(4, recv(reader_.Get(), bytes.data(), bytes.size(), MSG_TRUNC | MSG_WAITALL));
    for (char byte : bytes) EXPECT_EQ('A', byte);
    EXPECT_EQ(1, Mark());
    Read("Y", MSG_OOB); Read("def");
}

TEST_P(TcpUrgent, UrgentOnlySetsPriNotOrdinaryReadiness) {
    Queue("", "!", "");
    pollfd ready{reader_.Get(), POLLIN | POLLPRI, 0};
    ASSERT_EQ(1, poll(&ready, 1, 0));
    EXPECT_EQ(0, ready.revents & POLLIN); EXPECT_NE(0, ready.revents & POLLPRI);
    Read("!", MSG_OOB);
    ready.revents = 0;
    EXPECT_EQ(0, poll(&ready, 1, 0));
    char byte;
    EXPECT_EQ(-1, recv(reader_.Get(), &byte, 1, MSG_DONTWAIT)); EXPECT_EQ(EAGAIN, errno);
    EXPECT_EQ(0, Mark());
}

TEST_P(TcpUrgent, LatestMarkOverwritesEarlierUrgentWithoutASecondQueue) {
    Queue("a", "X", "b");
    ASSERT_EQ(1, send(writer_.Get(), "Y", 1, MSG_OOB));
    ASSERT_EQ(1, send(writer_.Get(), "c", 1, 0));
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(2);
    while (Inq() != 3 && std::chrono::steady_clock::now() < deadline) usleep(1000);
    ASSERT_EQ(3, Inq());
    Read("Y", MSG_OOB); Read("aXb"); Read("c");
}

TEST_P(TcpUrgent, OobPushesExistingCorkPrefixWithoutReordering) {
    int enabled = 1;
    ASSERT_EQ(0, setsockopt(writer_.Get(), IPPROTO_TCP, TCP_CORK, &enabled, sizeof(enabled)));
    ASSERT_EQ(3, send(writer_.Get(), "abc", 3, 0));
    ASSERT_EQ(1, send(writer_.Get(), "!", 1, MSG_OOB));
    WaitPri();
    Read("abc"); Read("!", MSG_OOB);
}

TEST_P(TcpUrgent, VectoredOobMarksTheLastByteOfTheLogicalSend) {
    char first[] = "ab", second[] = "XY";
    iovec vectors[] = {{first, 2}, {second, 2}};
    msghdr message{}; message.msg_iov = vectors; message.msg_iovlen = 2;
    ASSERT_EQ(4, sendmsg(writer_.Get(), &message, MSG_OOB));
    WaitPri();
    Read("Y", MSG_OOB); Read("abX");
}

TEST_P(TcpUrgent, BoundedScalarSendChunksKeepTheFinalUrgentMark) {
    std::vector<char> payload(3 * 4096 + 1, 'a'); payload.back() = '!';
    ASSERT_EQ(static_cast<ssize_t>(payload.size()), send(writer_.Get(), payload.data(), payload.size(), MSG_OOB));
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(2);
    while (Inq() != static_cast<int>(payload.size() - 1) && std::chrono::steady_clock::now() < deadline) usleep(1000);
    ASSERT_EQ(static_cast<int>(payload.size() - 1), Inq());
    Read("!", MSG_OOB);
    std::vector<char> actual(payload.size());
    ASSERT_EQ(static_cast<ssize_t>(payload.size() - 1), recv(reader_.Get(), actual.data(), actual.size(), MSG_WAITALL));
    EXPECT_EQ(0, memcmp(payload.data(), actual.data(), payload.size() - 1));
}

class SignalMask {
 public:
    SignalMask() { sigemptyset(&set_); sigaddset(&set_, SIGURG); valid_ = sigprocmask(SIG_BLOCK, &set_, &old_) == 0; }
    ~SignalMask() { if (valid_) { timespec zero{}; siginfo_t info{}; while (sigtimedwait(&set_, &info, &zero) >= 0) {} sigprocmask(SIG_SETMASK, &old_, nullptr); } }
    bool Valid() const { return valid_; }
    int Wait(siginfo_t* info) { timespec timeout{2, 0}; return sigtimedwait(&set_, info, &timeout); }
 private:
    bool valid_ = false;
    sigset_t set_{}, old_{};
};
TEST_P(TcpUrgent, SigurgUsesOwnerWithoutOasyncAndIgnoresFsetsig) {
    SignalMask mask;
    ASSERT_TRUE(mask.Valid());
    ASSERT_EQ(0, fcntl(reader_.Get(), F_SETOWN, getpid()));
    ASSERT_EQ(0, fcntl(reader_.Get(), F_SETSIG, SIGRTMIN));
    ASSERT_EQ(0, fcntl(reader_.Get(), F_GETFL) & O_ASYNC);
    Queue("", "!", "");
    siginfo_t info{};
    ASSERT_EQ(SIGURG, mask.Wait(&info));
    EXPECT_EQ(SIGURG, info.si_signo);
    EXPECT_EQ(SI_KERNEL, info.si_code);
    Read("!", MSG_OOB);
}

TEST_P(TcpUrgent, UrgentOwnerSurvivesDupAndScmRightsWithoutOasync) {
    SignalMask mask;
    ASSERT_TRUE(mask.Valid());
    ASSERT_EQ(0, fcntl(reader_.Get(), F_SETOWN, getpid()));
    int duplicate = dup(reader_.Get());
    ASSERT_GE(duplicate, 0);
    ASSERT_EQ(0, close(reader_.Release()));
    reader_.Set(duplicate);
    Queue("", "!", "");
    siginfo_t info{};
    ASSERT_EQ(SIGURG, mask.Wait(&info));
    Read("!", MSG_OOB);

    int channels[2];
    ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_DGRAM, 0, channels));
    Fd sender(channels[0]), receiver(channels[1]);
    char marker = 'x'; iovec vector{&marker, 1};
    std::array<char, CMSG_SPACE(sizeof(int))> control{};
    msghdr message{}; message.msg_iov = &vector; message.msg_iovlen = 1;
    message.msg_control = control.data(); message.msg_controllen = control.size();
    auto* header = CMSG_FIRSTHDR(&message);
    header->cmsg_level = SOL_SOCKET; header->cmsg_type = SCM_RIGHTS; header->cmsg_len = CMSG_LEN(sizeof(int));
    const int original = reader_.Get();
    memcpy(CMSG_DATA(header), &original, sizeof(original));
    ASSERT_EQ(1, sendmsg(sender.Get(), &message, 0));
    control.fill(0); message.msg_controllen = control.size();
    ASSERT_EQ(1, recvmsg(receiver.Get(), &message, 0));
    header = CMSG_FIRSTHDR(&message);
    ASSERT_NE(nullptr, header); ASSERT_EQ(SCM_RIGHTS, header->cmsg_type);
    int received = -1; memcpy(&received, CMSG_DATA(header), sizeof(received));
    ASSERT_GE(received, 0);
    ASSERT_EQ(0, close(reader_.Release()));
    reader_.Set(received);
    EXPECT_EQ(getpid(), fcntl(reader_.Get(), F_GETOWN));
    EXPECT_EQ(0, fcntl(reader_.Get(), F_GETFL) & O_ASYNC);
    Queue("", "@", "");
    ASSERT_EQ(SIGURG, mask.Wait(&info));
    Read("@", MSG_OOB);
}

TEST_P(TcpUrgent, UrgentTidOwnerTargetsANonleaderThread) {
    SignalMask mask;
    ASSERT_TRUE(mask.Valid());
    std::promise<int> thread_id, received_signal;
    auto id = thread_id.get_future(), received = received_signal.get_future();
    std::thread target([&] {
        thread_id.set_value(static_cast<int>(syscall(SYS_gettid)));
        sigset_t wanted; sigemptyset(&wanted); sigaddset(&wanted, SIGURG);
        siginfo_t info{}; timespec timeout{2, 0};
        received_signal.set_value(sigtimedwait(&wanted, &info, &timeout));
    });
    const int tid = id.get();
    EXPECT_NE(getpid(), tid);
    f_owner_ex owner{F_OWNER_TID, tid};
    const int configured = fcntl(reader_.Get(), F_SETOWN_EX, &owner);
    EXPECT_EQ(0, configured) << errno;
    if (configured == 0) {
        f_owner_ex actual{};
        EXPECT_EQ(0, fcntl(reader_.Get(), F_GETOWN_EX, &actual));
        EXPECT_EQ(F_OWNER_TID, actual.type); EXPECT_EQ(tid, actual.pid);
        Queue("", "!", "");
    }
    const int signal = received.get();
    target.join();
    EXPECT_EQ(SIGURG, signal);
    if (configured == 0) Read("!", MSG_OOB);
}

TEST_P(TcpUrgent, UrgentPgidOwnerOnlyTargetsAPrivateChildGroup) {
    SignalMask mask;
    ASSERT_TRUE(mask.Valid());
    int status_pipe[2];
    ASSERT_EQ(0, pipe(status_pipe));
    Fd output(status_pipe[0]), input(status_pipe[1]);
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(output.Release());
        if (setpgid(0, 0) != 0) _exit(2);
        const pid_t group = getpgrp();
        if (group != getpid() || write(input.Get(), &group, sizeof(group)) != sizeof(group)) _exit(3);
        sigset_t wanted; sigemptyset(&wanted); sigaddset(&wanted, SIGURG);
        siginfo_t info{}; timespec timeout{2, 0};
        const int signal = sigtimedwait(&wanted, &info, &timeout);
        int result[] = {signal, info.si_code};
        if (write(input.Get(), result, sizeof(result)) != sizeof(result)) _exit(4);
        _exit(signal == SIGURG ? 0 : 5);
    }
    close(input.Release());
    pollfd ready{output.Get(), POLLIN, 0};
    const int ready_result = poll(&ready, 1, 3000);
    pid_t group = 0;
    const ssize_t ready_bytes = ready_result == 1 ? read(output.Get(), &group, sizeof(group)) : -1;
    int configured = -1;
    if (ready_bytes == sizeof(group) && group == child) {
        // Never register or signal any ambient process group.
        configured = fcntl(reader_.Get(), F_SETOWN, -group);
        EXPECT_EQ(0, configured) << errno;
        if (configured == 0) {
            EXPECT_EQ(-group, fcntl(reader_.Get(), F_GETOWN));
            Queue("", "!", "");
        }
    }
    ready.revents = 0;
    const int completed = poll(&ready, 1, 3000);
    int result[2]{};
    const ssize_t received = completed == 1 ? read(output.Get(), result, sizeof(result)) : -1;
    if (completed != 1) kill(child, SIGKILL);
    int status = 0;
    EXPECT_EQ(child, waitpid(child, &status, 0));
    ASSERT_EQ(sizeof(group), ready_bytes);
    ASSERT_EQ(child, group);
    ASSERT_EQ(0, configured);
    ASSERT_EQ(sizeof(result), received);
    EXPECT_EQ(SIGURG, result[0]); EXPECT_EQ(SI_KERNEL, result[1]);
    EXPECT_TRUE(WIFEXITED(status)); EXPECT_EQ(0, WEXITSTATUS(status));
    Read("!", MSG_OOB);
}

INSTANTIATE_TEST_SUITE_P(DualStack, TcpUrgent, testing::Values(AF_INET, AF_INET6));
} // namespace
int main(int argc, char** argv) { testing::InitGoogleTest(&argc, argv); return RUN_ALL_TESTS(); }
