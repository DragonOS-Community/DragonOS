// IPv4 SO_BINDTODEVICE semantics, also runnable against the Linux fixture.
#include <gtest/gtest.h>
#include <arpa/inet.h>
#include <net/if.h>
#include <poll.h>
#include <sys/socket.h>
#include <unistd.h>

#include <cerrno>
#include <cstring>
#include <initializer_list>

namespace {
class Fd {
 public:
    explicit Fd(int type = SOCK_STREAM) : value_(socket(AF_INET, type, 0)) {}
    ~Fd() { if (value_ >= 0) close(value_); }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int get() const { return value_; }
 private:
    int value_;
};

void SetDevice(int fd, const char* device) {
    ASSERT_EQ(setsockopt(fd, SOL_SOCKET, SO_BINDTODEVICE,
                         device, strlen(device) + 1), 0) << strerror(errno);
}

void CheckDevice(int fd, const char* expected) {
    char device[IFNAMSIZ]{};
    socklen_t length = sizeof(device);
    ASSERT_EQ(getsockopt(fd, SOL_SOCKET, SO_BINDTODEVICE, device, &length), 0);
    EXPECT_EQ(length, expected[0] ? strlen(expected) + 1 : 0u);
    EXPECT_STREQ(device, expected);
}

void FinishConnect(int fd, const sockaddr_in& peer) {
    const int result = connect(fd, reinterpret_cast<const sockaddr*>(&peer), sizeof(peer));
    if (result != 0) {
        ASSERT_EQ(errno, EINPROGRESS) << strerror(errno);
        pollfd event{fd, POLLOUT, 0};
        ASSERT_EQ(poll(&event, 1, 3000), 1);
    }
    int error = -1;
    socklen_t length = sizeof(error);
    ASSERT_EQ(getsockopt(fd, SOL_SOCKET, SO_ERROR, &error, &length), 0);
    ASSERT_EQ(error, 0) << strerror(error);
    sockaddr_in remote{};
    length = sizeof(remote);
    ASSERT_EQ(getpeername(fd, reinterpret_cast<sockaddr*>(&remote), &length), 0);
    EXPECT_EQ(remote.sin_addr.s_addr, peer.sin_addr.s_addr);
    EXPECT_EQ(remote.sin_port, peer.sin_port);
}

TEST(TcpDeviceBinding, SetGetUnknownClearAndShortBuffer) {
    Fd socket;
    ASSERT_GE(socket.get(), 0);
    ASSERT_NE(if_nametoindex("lo"), 0u);
    ASSERT_NO_FATAL_FAILURE(CheckDevice(socket.get(), ""));
    ASSERT_NO_FATAL_FAILURE(SetDevice(socket.get(), "lo"));
    ASSERT_NO_FATAL_FAILURE(CheckDevice(socket.get(), "lo"));

    // Linux requires IFNAMSIZ, even when the actual name fits a shorter buffer.
    char short_buffer[IFNAMSIZ - 1]{};
    socklen_t length = sizeof(short_buffer);
    EXPECT_EQ(getsockopt(socket.get(), SOL_SOCKET, SO_BINDTODEVICE,
                         short_buffer, &length), -1);
    EXPECT_EQ(errno, EINVAL);
    constexpr char missing[] = "ngc009_missing";
    ASSERT_EQ(if_nametoindex(missing), 0u);
    EXPECT_EQ(setsockopt(socket.get(), SOL_SOCKET, SO_BINDTODEVICE,
                         missing, sizeof(missing)), -1);
    EXPECT_EQ(errno, ENODEV);
    ASSERT_NO_FATAL_FAILURE(CheckDevice(socket.get(), "lo"));

    ASSERT_NO_FATAL_FAILURE(SetDevice(socket.get(), ""));
    ASSERT_NO_FATAL_FAILURE(CheckDevice(socket.get(), ""));
    length = 0;
    ASSERT_EQ(getsockopt(socket.get(), SOL_SOCKET, SO_BINDTODEVICE,
                         short_buffer, &length), 0);
    EXPECT_EQ(length, 0u);
    ASSERT_NO_FATAL_FAILURE(SetDevice(socket.get(), "lo"));
    ASSERT_EQ(setsockopt(socket.get(), SOL_SOCKET, SO_BINDTODEVICE, nullptr, 0), 0);
    ASSERT_NO_FATAL_FAILURE(CheckDevice(socket.get(), ""));
}

class TcpDeviceBindingPorts : public testing::Test {
 protected:
    void SetUp() override {
        ASSERT_NE(if_nametoindex("veth1"), 0u) << "requires the standard veth fixture";
        ASSERT_NE(if_nametoindex("veth2"), 0u);
    }
    void BindEphemeral(int fd, sockaddr_in* local) {
        local->sin_family = AF_INET;
        local->sin_addr.s_addr = htonl(INADDR_ANY);
        ASSERT_EQ(bind(fd, reinterpret_cast<sockaddr*>(local), sizeof(*local)), 0);
        socklen_t length = sizeof(*local);
        ASSERT_EQ(getsockname(fd, reinterpret_cast<sockaddr*>(local), &length), 0);
        ASSERT_NE(local->sin_port, 0);
    }
};

TEST_F(TcpDeviceBindingPorts, DifferentDevicesAllowSameAddressAndPort) {
    Fd first, second;
    ASSERT_GE(first.get(), 0);
    ASSERT_GE(second.get(), 0);
    ASSERT_NO_FATAL_FAILURE(SetDevice(first.get(), "veth1"));
    ASSERT_NO_FATAL_FAILURE(SetDevice(second.get(), "veth2"));
    sockaddr_in local{};
    ASSERT_NO_FATAL_FAILURE(BindEphemeral(first.get(), &local));
    ASSERT_EQ(bind(second.get(), reinterpret_cast<sockaddr*>(&local), sizeof(local)), 0)
        << strerror(errno);
    ASSERT_EQ(listen(first.get(), 1), 0);
    ASSERT_EQ(listen(second.get(), 1), 0);
}

TEST_F(TcpDeviceBindingPorts, SameDeviceOrUnboundDeviceConflicts) {
    for (const char* initial_device : {"veth1", ""}) {
        Fd first;
        ASSERT_GE(first.get(), 0);
        ASSERT_NO_FATAL_FAILURE(SetDevice(first.get(), initial_device));
        sockaddr_in local{};
        ASSERT_NO_FATAL_FAILURE(BindEphemeral(first.get(), &local));
        for (const char* other_device : {"veth1", ""}) {
            Fd other;
            ASSERT_GE(other.get(), 0);
            ASSERT_NO_FATAL_FAILURE(SetDevice(other.get(), other_device));
            EXPECT_EQ(bind(other.get(), reinterpret_cast<sockaddr*>(&local), sizeof(local)), -1);
            EXPECT_EQ(errno, EADDRINUSE);
        }
    }
}

TEST_F(TcpDeviceBindingPorts, ChangingDeviceUpdatesFutureBindConflicts) {
    Fd first, old_device, new_device;
    ASSERT_GE(first.get(), 0);
    ASSERT_GE(old_device.get(), 0);
    ASSERT_GE(new_device.get(), 0);
    ASSERT_NO_FATAL_FAILURE(SetDevice(first.get(), "veth1"));
    sockaddr_in local{};
    ASSERT_NO_FATAL_FAILURE(BindEphemeral(first.get(), &local));
    ASSERT_NO_FATAL_FAILURE(SetDevice(first.get(), "veth2"));
    ASSERT_NO_FATAL_FAILURE(SetDevice(old_device.get(), "veth1"));
    ASSERT_EQ(bind(old_device.get(), reinterpret_cast<sockaddr*>(&local), sizeof(local)), 0);
    ASSERT_NO_FATAL_FAILURE(SetDevice(new_device.get(), "veth2"));
    EXPECT_EQ(bind(new_device.get(), reinterpret_cast<sockaddr*>(&local), sizeof(local)), -1);
    EXPECT_EQ(errno, EADDRINUSE);
    // Linux permits changing the device after bind even if that creates an
    // overlap; setsockopt must not perform a second bind conflict check.
    ASSERT_NO_FATAL_FAILURE(SetDevice(first.get(), "veth1"));
    ASSERT_NO_FATAL_FAILURE(CheckDevice(first.get(), "veth1"));
}

TEST_F(TcpDeviceBindingPorts, SelfConnectHonorsDeviceConstraint) {
    for (const char* device : {"lo", "veth1"}) {
        SCOPED_TRACE(device);
        Fd client(SOCK_STREAM | SOCK_NONBLOCK);
        ASSERT_GE(client.get(), 0);
        ASSERT_NO_FATAL_FAILURE(SetDevice(client.get(), device));
        sockaddr_in local{};
        local.sin_family = AF_INET;
        local.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        ASSERT_EQ(bind(client.get(), reinterpret_cast<sockaddr*>(&local), sizeof(local)), 0);
        socklen_t length = sizeof(local);
        ASSERT_EQ(getsockname(client.get(), reinterpret_cast<sockaddr*>(&local), &length), 0);
        if (strcmp(device, "lo") == 0) {
            ASSERT_NO_FATAL_FAILURE(FinishConnect(client.get(), local));
        } else {
            // Linux rejects a loopback source constrained to a non-loopback
            // output device, including the otherwise valid self-connect case.
            ASSERT_EQ(connect(client.get(), reinterpret_cast<sockaddr*>(&local), sizeof(local)), -1);
            EXPECT_EQ(errno, EINVAL);
        }
    }
}

TEST_F(TcpDeviceBindingPorts, ExplicitLoopbackSourceRejectsNonLoopbackOutput) {
    Fd client(SOCK_STREAM | SOCK_NONBLOCK);
    ASSERT_GE(client.get(), 0);
    ASSERT_NO_FATAL_FAILURE(SetDevice(client.get(), "veth1"));
    sockaddr_in local{};
    local.sin_family = AF_INET;
    local.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    ASSERT_EQ(bind(client.get(), reinterpret_cast<sockaddr*>(&local), sizeof(local)), 0);
    socklen_t length = sizeof(local);
    ASSERT_EQ(getsockname(client.get(), reinterpret_cast<sockaddr*>(&local), &length), 0);
    sockaddr_in peer = local;
    peer.sin_addr.s_addr = htonl(0x6f6f0b02);  // Standard fixture veth2 address.
    ASSERT_EQ(connect(client.get(), reinterpret_cast<sockaddr*>(&peer), sizeof(peer)), -1);
    EXPECT_EQ(errno, EINVAL);
    sockaddr_in after{};
    length = sizeof(after);
    ASSERT_EQ(getsockname(client.get(), reinterpret_cast<sockaddr*>(&after), &length), 0);
    EXPECT_EQ(after.sin_addr.s_addr, local.sin_addr.s_addr);
    EXPECT_EQ(after.sin_port, local.sin_port);
    ASSERT_NO_FATAL_FAILURE(CheckDevice(client.get(), "veth1"));
}

TEST_F(TcpDeviceBindingPorts, CrossDeviceSelfConnectRequiresNetworkHandshake) {
    Fd client(SOCK_STREAM | SOCK_NONBLOCK);
    ASSERT_GE(client.get(), 0);
    sockaddr_in local{};
    local.sin_family = AF_INET;
    local.sin_addr.s_addr = htonl(0x6f6f0b01);  // Standard fixture veth1 address.
    ASSERT_EQ(bind(client.get(), reinterpret_cast<sockaddr*>(&local), sizeof(local)), 0);
    socklen_t length = sizeof(local);
    ASSERT_EQ(getsockname(client.get(), reinterpret_cast<sockaddr*>(&local), &length), 0);
    ASSERT_NO_FATAL_FAILURE(SetDevice(client.get(), "veth2"));
    // Equal endpoints alone do not imply a local route: the device constraint
    // selects the veth2 output path. Linux starts a real asynchronous handshake.
    // Do not wait for timeout, whose duration depends on TCP retransmission.
    ASSERT_EQ(connect(client.get(), reinterpret_cast<sockaddr*>(&local), sizeof(local)), -1);
    EXPECT_EQ(errno, EINPROGRESS);
}

TEST_F(TcpDeviceBindingPorts, EstablishedSelfConnectHonorsDeviceChanges) {
    Fd client(SOCK_STREAM | SOCK_NONBLOCK);
    ASSERT_GE(client.get(), 0);
    ASSERT_NO_FATAL_FAILURE(SetDevice(client.get(), "lo"));
    sockaddr_in local{};
    local.sin_family = AF_INET;
    local.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    ASSERT_EQ(bind(client.get(), reinterpret_cast<sockaddr*>(&local), sizeof(local)), 0);
    socklen_t length = sizeof(local);
    ASSERT_EQ(getsockname(client.get(), reinterpret_cast<sockaddr*>(&local), &length), 0);
    ASSERT_NO_FATAL_FAILURE(FinishConnect(client.get(), local));

    // First leave an already received byte queued across the device change.
    const char old_byte = 'a';
    ASSERT_EQ(send(client.get(), &old_byte, 1, 0), 1);
    pollfd event{client.get(), POLLIN, 0};
    ASSERT_EQ(poll(&event, 1, 3000), 1);
    ASSERT_NE(event.revents & POLLIN, 0);
    ASSERT_NO_FATAL_FAILURE(SetDevice(client.get(), "veth1"));
    char received = 0;
    ASSERT_EQ(recv(client.get(), &received, 1, 0), 1);
    EXPECT_EQ(received, old_byte);

    // send() may queue data, but a non-loopback device must prevent delivery
    // to the loopback endpoint even for a connection with equal endpoints.
    const char new_byte = 'b';
    ASSERT_EQ(send(client.get(), &new_byte, 1, 0), 1);
    event.revents = 0;
    EXPECT_EQ(poll(&event, 1, 150), 0);
    ASSERT_EQ(recv(client.get(), &received, 1, 0), -1);
    EXPECT_EQ(errno, EAGAIN);

    // Clearing the constraint restores routing. The byte already queued for
    // transmission must arrive through TCP retransmission without another send.
    ASSERT_NO_FATAL_FAILURE(SetDevice(client.get(), ""));
    event.revents = 0;
    ASSERT_EQ(poll(&event, 1, 5000), 1);
    ASSERT_NE(event.revents & POLLIN, 0);
    ASSERT_EQ(recv(client.get(), &received, 1, 0), 1);
    EXPECT_EQ(received, new_byte);
}

TEST_F(TcpDeviceBindingPorts, WildcardBindConnectPreservesPortAndSelectsLoopbackSource) {
    Fd listener(SOCK_STREAM | SOCK_NONBLOCK), client(SOCK_STREAM | SOCK_NONBLOCK);
    ASSERT_GE(listener.get(), 0);
    ASSERT_GE(client.get(), 0);
    ASSERT_NO_FATAL_FAILURE(SetDevice(listener.get(), "lo"));
    ASSERT_NO_FATAL_FAILURE(SetDevice(client.get(), "lo"));
    sockaddr_in server{};
    server.sin_family = AF_INET;
    server.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    ASSERT_EQ(bind(listener.get(), reinterpret_cast<sockaddr*>(&server), sizeof(server)), 0);
    socklen_t length = sizeof(server);
    ASSERT_EQ(getsockname(listener.get(), reinterpret_cast<sockaddr*>(&server), &length), 0);
    ASSERT_EQ(listen(listener.get(), 1), 0);
    sockaddr_in before{};
    ASSERT_NO_FATAL_FAILURE(BindEphemeral(client.get(), &before));
    ASSERT_EQ(before.sin_addr.s_addr, htonl(INADDR_ANY));
    ASSERT_NO_FATAL_FAILURE(FinishConnect(client.get(), server));
    sockaddr_in after{};
    length = sizeof(after);
    ASSERT_EQ(getsockname(client.get(), reinterpret_cast<sockaddr*>(&after), &length), 0);
    EXPECT_EQ(after.sin_port, before.sin_port);
    EXPECT_EQ(after.sin_addr.s_addr, htonl(INADDR_LOOPBACK));
    ASSERT_NO_FATAL_FAILURE(CheckDevice(client.get(), "lo"));
    pollfd event{listener.get(), POLLIN, 0};
    ASSERT_EQ(poll(&event, 1, 3000), 1);
    const int accepted = accept(listener.get(), nullptr, nullptr);
    ASSERT_GE(accepted, 0) << strerror(errno);
    EXPECT_EQ(close(accepted), 0);
}
}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
