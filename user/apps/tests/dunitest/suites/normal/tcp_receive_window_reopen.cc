#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <poll.h>
#include <sys/socket.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <chrono>
#include <cstdint>
#include <cstring>
#include <string>

namespace {

class FdGuard {
  public:
    explicit FdGuard(int fd = -1) : fd_(fd) {}
    FdGuard(const FdGuard&) = delete;
    FdGuard& operator=(const FdGuard&) = delete;
    FdGuard(FdGuard&& other) noexcept : fd_(other.fd_) { other.fd_ = -1; }
    FdGuard& operator=(FdGuard&& other) noexcept {
        if (this != &other) {
            if (fd_ >= 0) {
                close(fd_);
            }
            fd_ = other.fd_;
            other.fd_ = -1;
        }
        return *this;
    }
    ~FdGuard() {
        if (fd_ >= 0) {
            close(fd_);
        }
    }

    int Get() const { return fd_; }

  private:
    int fd_;
};

std::string ErrnoString(int err) {
    return std::to_string(err) + " (" + std::strerror(err) + ")";
}

struct TcpPair {
    FdGuard listener;
    FdGuard sender;
    FdGuard receiver;
};

TcpPair MakeTcpPair() {
    FdGuard listener(socket(AF_INET, SOCK_STREAM, 0));
    EXPECT_GE(listener.Get(), 0) << ErrnoString(errno);

    sockaddr_in address {};
    address.sin_family = AF_INET;
    address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    address.sin_port = 0;
    EXPECT_EQ(bind(listener.Get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), 0)
            << ErrnoString(errno);
    EXPECT_EQ(listen(listener.Get(), 1), 0) << ErrnoString(errno);

    socklen_t address_len = sizeof(address);
    EXPECT_EQ(getsockname(listener.Get(), reinterpret_cast<sockaddr*>(&address), &address_len), 0)
            << ErrnoString(errno);

    FdGuard sender(socket(AF_INET, SOCK_STREAM, 0));
    EXPECT_GE(sender.Get(), 0) << ErrnoString(errno);
    EXPECT_EQ(connect(sender.Get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), 0)
            << ErrnoString(errno);

    FdGuard receiver(accept(listener.Get(), nullptr, nullptr));
    EXPECT_GE(receiver.Get(), 0) << ErrnoString(errno);

    return {std::move(listener), std::move(sender), std::move(receiver)};
}

uint8_t PatternByte(size_t offset) {
    return static_cast<uint8_t>((offset * 131u + 17u) & 0xffu);
}

void FillPattern(std::array<uint8_t, 4096>* buffer, size_t offset) {
    for (size_t index = 0; index < buffer->size(); ++index) {
        (*buffer)[index] = PatternByte(offset + index);
    }
}

void VerifyPattern(const uint8_t* buffer, size_t length, size_t offset) {
    for (size_t index = 0; index < length; ++index) {
        ASSERT_EQ(buffer[index], PatternByte(offset + index))
                << "data mismatch at stream offset " << offset + index;
    }
}

}  // namespace

TEST(TcpReceiveWindowReopen, ReceiverProgressMakesBlockedSenderWritable) {
    for (int iteration = 0; iteration < 4; ++iteration) {
        TcpPair pair = MakeTcpPair();
        ASSERT_GE(pair.sender.Get(), 0);
        ASSERT_GE(pair.receiver.Get(), 0);

        const int flags = fcntl(pair.sender.Get(), F_GETFL, 0);
        ASSERT_GE(flags, 0) << ErrnoString(errno);
        ASSERT_EQ(fcntl(pair.sender.Get(), F_SETFL, flags | O_NONBLOCK), 0)
                << ErrnoString(errno);

        std::array<uint8_t, 4096> write_buffer {};
        size_t bytes_sent = 0;
        bool reached_backpressure = false;
        constexpr size_t kSafetyLimit = 8 * 1024 * 1024;

        while (bytes_sent < kSafetyLimit) {
            FillPattern(&write_buffer, bytes_sent);
            const ssize_t written =
                    send(pair.sender.Get(), write_buffer.data(), write_buffer.size(), MSG_DONTWAIT);
            if (written > 0) {
                bytes_sent += static_cast<size_t>(written);
                continue;
            }
            if (written < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
                reached_backpressure = true;
                break;
            }
            FAIL() << "send failed while filling the receive window: " << ErrnoString(errno);
        }
        ASSERT_TRUE(reached_backpressure)
                << "sender did not observe backpressure before the safety limit";

        pollfd blocked {.fd = pair.sender.Get(), .events = POLLOUT, .revents = 0};
        ASSERT_EQ(poll(&blocked, 1, 50), 0)
                << "sender did not remain backpressured while the receiver was idle, revents="
                << blocked.revents;

        const int receiver_flags = fcntl(pair.receiver.Get(), F_GETFL, 0);
        ASSERT_GE(receiver_flags, 0) << ErrnoString(errno);
        ASSERT_EQ(fcntl(pair.receiver.Get(), F_SETFL, receiver_flags | O_NONBLOCK), 0)
                << ErrnoString(errno);

        std::array<uint8_t, 32768> read_buffer {};
        size_t bytes_consumed = 0;
        const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(2);

        // Establish the causal boundary explicitly: drain only receiver data
        // that is already available before observing sender writability again.
        // A single fixed-size recv is insufficient on Linux because POLLOUT
        // depends on the dynamically sized send-buffer low-water threshold.
        while (std::chrono::steady_clock::now() < deadline) {
            const ssize_t consumed = recv(
                    pair.receiver.Get(), read_buffer.data(), read_buffer.size(), MSG_DONTWAIT);
            if (consumed < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
                if (bytes_consumed > 0) {
                    break;
                }
                const auto remaining = std::chrono::duration_cast<std::chrono::milliseconds>(
                        deadline - std::chrono::steady_clock::now());
                pollfd readable {.fd = pair.receiver.Get(), .events = POLLIN, .revents = 0};
                const int ready = poll(
                        &readable,
                        1,
                        static_cast<int>(std::max<int64_t>(1, remaining.count())));
                ASSERT_GE(ready, 0) << ErrnoString(errno);
                ASSERT_EQ(readable.revents & (POLLERR | POLLHUP | POLLNVAL), 0);
                continue;
            }
            ASSERT_GT(consumed, 0) << ErrnoString(errno);
            VerifyPattern(read_buffer.data(), static_cast<size_t>(consumed), bytes_consumed);
            bytes_consumed += static_cast<size_t>(consumed);
        }
        ASSERT_GT(bytes_consumed, 0u) << "receiver did not consume data before the deadline";

        const auto writable_deadline =
                std::chrono::steady_clock::now() + std::chrono::seconds(2);
        pollfd writable {.fd = pair.sender.Get(), .events = POLLOUT, .revents = 0};
        const auto writable_remaining = std::chrono::duration_cast<std::chrono::milliseconds>(
                writable_deadline - std::chrono::steady_clock::now());
        const int writable_ready = poll(
                &writable,
                1,
                static_cast<int>(std::max<int64_t>(1, writable_remaining.count())));
        ASSERT_GE(writable_ready, 0) << ErrnoString(errno);
        ASSERT_EQ(writable.revents & (POLLERR | POLLHUP | POLLNVAL), 0);
        EXPECT_NE(writable.revents & POLLOUT, 0)
                << "sender did not become writable after receiver consumed "
                << bytes_consumed << " bytes";

        const auto drain_deadline =
                std::chrono::steady_clock::now() + std::chrono::seconds(2);
        while (bytes_consumed < bytes_sent &&
               std::chrono::steady_clock::now() < drain_deadline) {
            const auto remaining = std::chrono::duration_cast<std::chrono::milliseconds>(
                    drain_deadline - std::chrono::steady_clock::now());
            pollfd readable {.fd = pair.receiver.Get(), .events = POLLIN, .revents = 0};
            const int ready =
                    poll(&readable, 1, static_cast<int>(std::max<int64_t>(1, remaining.count())));
            ASSERT_GE(ready, 0) << ErrnoString(errno);
            ASSERT_EQ(readable.revents & (POLLERR | POLLHUP | POLLNVAL), 0);
            if ((readable.revents & POLLIN) == 0) {
                continue;
            }

            const size_t wanted =
                    std::min(read_buffer.size(), bytes_sent - bytes_consumed);
            const ssize_t consumed =
                    recv(pair.receiver.Get(), read_buffer.data(), wanted, MSG_DONTWAIT);
            if (consumed < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
                continue;
            }
            ASSERT_GT(consumed, 0) << ErrnoString(errno);
            VerifyPattern(
                    read_buffer.data(), static_cast<size_t>(consumed), bytes_consumed);
            bytes_consumed += static_cast<size_t>(consumed);
        }

        EXPECT_EQ(bytes_consumed, bytes_sent)
                << "receiver did not drain all bytes accepted before backpressure";
    }
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
