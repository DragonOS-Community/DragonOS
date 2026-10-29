// 监听套接字 poll 就绪掩码的回归测试。
//
// Linux 6.6 语义（net/ipv4/tcp.c: tcp_poll）：
//   if (state == TCP_LISTEN)
//           return inet_csk_listen_poll(sk);
// 而 include/net/inet_connection_sock.h: inet_csk_listen_poll() 为
//   return !reqsk_queue_empty(&inet_csk(sk)->icsk_accept_queue) ? (EPOLLIN | EPOLLRDNORM) : 0;
// 也就是说 LISTEN 套接字的就绪掩码每次调用都重算，且永远不会出现 POLLHUP/POLLERR。
//
// DragonOS 的 pollee 是累积式的，bind() 时套接字（仍处于 Init 状态）就已注册进 iface 的
// 通知链，iface 每轮 poll 会无差别通知所有已绑定套接字。若 LISTEN 分支不清理残留的
// POLLHUP，则 poll(listen_fd, POLLIN, 0) 会在整个 listen 生命周期内恒返回 1。

#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <poll.h>
#include <sys/socket.h>
#include <unistd.h>

#include <cstring>
#include <string>

namespace {

class FdGuard {
  public:
    explicit FdGuard(int fd = -1) : fd_(fd) {}
    FdGuard(const FdGuard&) = delete;
    FdGuard& operator=(const FdGuard&) = delete;
    FdGuard(FdGuard&& other) noexcept : fd_(other.fd_) { other.fd_ = -1; }
    FdGuard& operator=(FdGuard&&) = delete;

    ~FdGuard() { Reset(); }

    int Get() const { return fd_; }

    void Reset(int fd = -1) {
        if (fd_ >= 0) {
            close(fd_);
        }
        fd_ = fd;
    }

  private:
    int fd_;
};

std::string ErrnoString(int err) {
    return std::to_string(err) + " (" + std::strerror(err) + ")";
}

int NewTcpSocket() {
    return socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
}

sockaddr_in LoopbackAddr(uint16_t port = 0) {
    sockaddr_in addr {};
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    addr.sin_port = htons(port);
    return addr;
}

// 绑定到 127.0.0.1:0 并返回内核选定的端口。
uint16_t BindLoopback(int fd) {
    sockaddr_in addr = LoopbackAddr();
    EXPECT_EQ(bind(fd, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)), 0)
            << "bind(127.0.0.1:0) failed: " << ErrnoString(errno);

    sockaddr_in bound {};
    socklen_t len = sizeof(bound);
    EXPECT_EQ(getsockname(fd, reinterpret_cast<sockaddr*>(&bound), &len), 0)
            << "getsockname failed: " << ErrnoString(errno);
    return ntohs(bound.sin_port);
}

struct PollOutcome {
    int ret;
    short revents;
};

PollOutcome PollFd(int fd, short events, int timeout_ms) {
    pollfd pfd = {fd, events, 0};
    int ret;
    do {
        ret = poll(&pfd, 1, timeout_ms);
    } while (ret < 0 && errno == EINTR);
    return PollOutcome {ret, pfd.revents};
}

// LISTEN 套接字永不报告 POLLHUP/POLLERR/POLLRDHUP（Linux 语义）。
void ExpectNoHangup(const PollOutcome& outcome) {
    EXPECT_EQ(outcome.revents & POLLHUP, 0) << "revents=" << outcome.revents;
    EXPECT_EQ(outcome.revents & POLLERR, 0) << "revents=" << outcome.revents;
}

FdGuard ConnectTo(uint16_t port) {
    FdGuard connector(NewTcpSocket());
    EXPECT_GE(connector.Get(), 0) << "socket failed: " << ErrnoString(errno);

    sockaddr_in addr = LoopbackAddr(port);
    EXPECT_EQ(connect(connector.Get(), reinterpret_cast<sockaddr*>(&addr), sizeof(addr)), 0)
            << "connect(127.0.0.1:" << port << ") failed: " << ErrnoString(errno);
    return connector;
}

}  // namespace

// 对齐 gVisor SimpleTcpSocketTest.PollAroundAccept：无连接时不就绪，连接到来后就绪，
// accept 之后重新变为不就绪。
TEST(TcpListenPollSemantics, ListenerPollAroundAccept) {
    FdGuard listener(NewTcpSocket());
    ASSERT_GE(listener.Get(), 0) << "socket failed: " << ErrnoString(errno);
    uint16_t port = BindLoopback(listener.Get());
    ASSERT_NE(port, 0);
    ASSERT_EQ(listen(listener.Get(), SOMAXCONN), 0) << "listen failed: " << ErrnoString(errno);

    PollOutcome before = PollFd(listener.Get(), POLLIN, 0);
    EXPECT_EQ(before.ret, 0) << "监听套接字没有 pending 连接时 poll 必须返回 0";
    ExpectNoHangup(before);

    FdGuard connector = ConnectTo(port);

    PollOutcome pending = PollFd(listener.Get(), POLLIN, 2000);
    EXPECT_EQ(pending.ret, 1) << "有 pending 连接时监听套接字必须就绪";
    EXPECT_NE(pending.revents & POLLIN, 0) << "revents=" << pending.revents;
    ExpectNoHangup(pending);

    FdGuard accepted(accept(listener.Get(), nullptr, nullptr));
    ASSERT_GE(accepted.Get(), 0) << "accept failed: " << ErrnoString(errno);

    PollOutcome after = PollFd(listener.Get(), POLLIN, 0);
    EXPECT_EQ(after.ret, 0) << "accept 之后监听套接字必须不再就绪";
    ExpectNoHangup(after);
}

// 确定性复现本次 CI 根因：同 iface 上关闭另一个 listener 会触发
// IfaceCommon::notify_all_bound_sockets()，此时仍处于 Init::Bound 的套接字会拿到
// EPOLLHUP。listen() 之后该 HUP 必须被重算清掉（Linux: LISTEN 不报 HUP）。
TEST(TcpListenPollSemantics, StaleHangupClearedAfterListen) {
    FdGuard target(NewTcpSocket());
    ASSERT_GE(target.Get(), 0) << "socket(target) failed: " << ErrnoString(errno);
    uint16_t port = BindLoopback(target.Get());
    ASSERT_NE(port, 0);

    {
        FdGuard other(NewTcpSocket());
        ASSERT_GE(other.Get(), 0) << "socket(other) failed: " << ErrnoString(errno);
        ASSERT_NE(BindLoopback(other.Get()), 0);
        ASSERT_EQ(listen(other.Get(), SOMAXCONN), 0)
                << "listen(other) failed: " << ErrnoString(errno);
        // other 在这里析构关闭：closing a listening socket 会让 iface 无差别通知
        // 所有已绑定套接字，其中就包括还停留在 Init::Bound 的 target。
    }

    ASSERT_EQ(listen(target.Get(), SOMAXCONN), 0) << "listen(target) failed: "
                                                 << ErrnoString(errno);

    PollOutcome outcome = PollFd(target.Get(), POLLIN, 0);
    EXPECT_EQ(outcome.ret, 0) << "listen() 之后不得残留 bind 窗口期打上的 POLLHUP";
    ExpectNoHangup(outcome);

    FdGuard connector = ConnectTo(port);
    PollOutcome pending = PollFd(target.Get(), POLLIN, 2000);
    EXPECT_EQ(pending.ret, 1) << "修复后 accept 就绪路径必须仍然可用";
    EXPECT_NE(pending.revents & POLLIN, 0) << "revents=" << pending.revents;
    ExpectNoHangup(pending);
}

// 覆盖 CI 中的竞态窗口：bind() 与 listen() 之间给后台 iface poll/NAPI 线程通知的机会，
// listen() 之后 poll 仍必须是完全重算的结果。
TEST(TcpListenPollSemantics, BackgroundPollDoesNotPoisonListener) {
    FdGuard listener(NewTcpSocket());
    ASSERT_GE(listener.Get(), 0) << "socket failed: " << ErrnoString(errno);
    uint16_t port = BindLoopback(listener.Get());
    ASSERT_NE(port, 0);

    usleep(50 * 1000);

    ASSERT_EQ(listen(listener.Get(), SOMAXCONN), 0) << "listen failed: " << ErrnoString(errno);

    PollOutcome outcome = PollFd(listener.Get(), POLLIN, 0);
    EXPECT_EQ(outcome.ret, 0) << "后台通知不得让监听套接字残留 POLLHUP";
    ExpectNoHangup(outcome);

    FdGuard connector = ConnectTo(port);
    PollOutcome pending = PollFd(listener.Get(), POLLIN, 2000);
    EXPECT_EQ(pending.ret, 1);
    EXPECT_NE(pending.revents & POLLIN, 0) << "revents=" << pending.revents;
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
