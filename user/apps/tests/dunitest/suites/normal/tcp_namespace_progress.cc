// Busy TCP on a non-loopback device must not hold an unrelated syscall hostage.
#include <gtest/gtest.h>
#include <arpa/inet.h>
#include <net/if.h>
#include <poll.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

#include <array>
#include <atomic>
#include <cerrno>
#include <chrono>
#include <cstring>
#include <new>
#include <string>

namespace {
using Clock = std::chrono::steady_clock;

struct Connection {
    int client = -1;
    int server = -1;
    ~Connection() {
        if (client >= 0) close(client);
        if (server >= 0) close(server);
    }
};

void Connect(in_addr address, Connection* result) {
    int listener = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    ASSERT_GE(listener, 0);
    struct ListenerGuard {
        int fd;
        ~ListenerGuard() { close(fd); }
    } guard{listener};
    sockaddr_in endpoint{};
    endpoint.sin_family = AF_INET;
    endpoint.sin_addr = address;
    ASSERT_EQ(bind(listener, reinterpret_cast<sockaddr*>(&endpoint), sizeof(endpoint)), 0);
    ASSERT_EQ(listen(listener, 1), 0);
    socklen_t length = sizeof(endpoint);
    ASSERT_EQ(getsockname(listener, reinterpret_cast<sockaddr*>(&endpoint), &length), 0);
    result->client = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    ASSERT_GE(result->client, 0);
    int connected = connect(result->client, reinterpret_cast<sockaddr*>(&endpoint), length);
    if (connected < 0) {
        ASSERT_EQ(errno, EINPROGRESS);
    }
    pollfd event{listener, POLLIN, 0};
    ASSERT_EQ(poll(&event, 1, 3000), 1);
    result->server = accept4(listener, nullptr, nullptr, SOCK_NONBLOCK);
    ASSERT_GE(result->server, 0);
    event = {result->client, POLLOUT, 0};
    ASSERT_EQ(poll(&event, 1, 3000), 1);
    int error = -1;
    length = sizeof(error);
    ASSERT_EQ(getsockopt(result->client, SOL_SOCKET, SO_ERROR, &error, &length), 0);
    ASSERT_EQ(error, 0);
}

struct Progress {
    std::atomic<unsigned long long> bytes{0};
    std::atomic<unsigned long long> maximum_probe_us{0};
    std::atomic<bool> stop{false};
};

// Stop the traffic producers before reaping a timed-out probe: even a broken
// until-quiescent implementation then gets a chance to leave its syscall.
class Children {
 public:
    explicit Children(Progress* progress) : progress_(progress) {}
    ~Children() {
        progress_->stop.store(true);
        for (pid_t pid : pids_) if (pid > 0) kill(pid, SIGKILL);
        const auto deadline = Clock::now() + std::chrono::seconds(5);
        for (pid_t& pid : pids_) {
            while (pid > 0 && Clock::now() < deadline) {
                int status;
                const pid_t done = waitpid(pid, &status, WNOHANG);
                if (done == pid || (done < 0 && errno == ECHILD)) { pid = -1; break; }
                usleep(1000);
            }
            EXPECT_LE(pid, 0) << "child remained in kernel after all traffic stopped";
        }
    }
    std::array<pid_t, 3> pids_{{-1, -1, -1}};
 private:
    Progress* progress_;
};

TEST(TcpNamespaceProgress, NonblockingIoReturnsDuringOtherDeviceTraffic) {
    ASSERT_NE(if_nametoindex("veth1"), 0u) << "requires the standard veth fixture";
    // Same standard fixture address as tcp_device_binding; avoid making this
    // transport progress regression depend on optional interface ioctls.
    const in_addr load_address{htonl(0x6f6f0b01)};  // 111.111.11.1

    std::array<Connection, 8> load;
    for (auto& connection : load) ASSERT_NO_FATAL_FAILURE(Connect(load_address, &connection));
    Connection probe;
    ASSERT_NO_FATAL_FAILURE(Connect(in_addr{htonl(INADDR_LOOPBACK)}, &probe));

    void* memory = mmap(nullptr, sizeof(Progress), PROT_READ | PROT_WRITE,
                        MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(memory, MAP_FAILED);
    struct Mapping {
        void* memory;
        ~Mapping() { munmap(memory, sizeof(Progress)); }
    } mapping{memory};
    auto* progress = new (memory) Progress;
    Children children(progress);
    for (int role = 0; role < 2; ++role) {
        children.pids_[role] = fork();
        ASSERT_GE(children.pids_[role], 0);
        if (children.pids_[role] == 0) {
            std::array<char, 32768> data{};
            const auto deadline = Clock::now() + std::chrono::seconds(15);
            while (!progress->stop.load() && Clock::now() < deadline) {
                for (auto& connection : load) {
                    ssize_t count = role == 0
                        ? send(connection.client, data.data(), data.size(), MSG_DONTWAIT | MSG_NOSIGNAL)
                        : recv(connection.server, data.data(), data.size(), MSG_DONTWAIT);
                    if (count < 0 && errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR)
                        _exit(10 + role);
                    if (role == 1 && count > 0) progress->bytes.fetch_add(count);
                }
            }
            _exit(0);
        }
    }
    const auto startup_deadline = Clock::now() + std::chrono::seconds(5);
    while (progress->bytes.load() < 65536 && Clock::now() < startup_deadline) usleep(1000);
    ASSERT_GE(progress->bytes.load(), 65536u) << "background transfer did not start";
    const auto before = progress->bytes.load();

    children.pids_[2] = fork();
    ASSERT_GE(children.pids_[2], 0);
    if (children.pids_[2] == 0) {
        const auto until = Clock::now() + std::chrono::seconds(1);
        unsigned int calls = 0;
        do {
            char byte = 'x';
            const auto start = Clock::now();
            if (recv(probe.client, &byte, 1, MSG_DONTWAIT) != -1 ||
                (errno != EAGAIN && errno != EWOULDBLOCK)) _exit(20);
            const auto sent = send(probe.client, &byte, 1, MSG_DONTWAIT | MSG_NOSIGNAL);
            if (sent != 1 && !(sent < 0 && (errno == EAGAIN || errno == EWOULDBLOCK))) _exit(21);
            const auto elapsed = Clock::now() - start;
            const auto micros = std::chrono::duration_cast<std::chrono::microseconds>(elapsed).count();
            if (static_cast<unsigned long long>(micros) > progress->maximum_probe_us.load())
                progress->maximum_probe_us.store(micros);
            if (elapsed > std::chrono::seconds(3)) _exit(22);
            ++calls;
        } while (Clock::now() < until);
        _exit(calls > 0 ? 0 : 23);
    }
    const auto deadline = Clock::now() + std::chrono::seconds(8);
    int status = 0;
    pid_t done = 0;
    while (Clock::now() < deadline) {
        done = waitpid(children.pids_[2], &status, WNOHANG);
        if (done != 0) break;
        usleep(1000);
    }
    if (done == children.pids_[2]) children.pids_[2] = -1;
    ASSERT_GT(done, 0) << "nonblocking syscall waited for namespace-wide quiescence";
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(WEXITSTATUS(status), 0);
    EXPECT_GT(progress->bytes.load(), before) << "no concurrent traffic during probe";
    RecordProperty("concurrent_bytes", std::to_string(progress->bytes.load() - before));
    RecordProperty("maximum_probe_us", std::to_string(progress->maximum_probe_us.load()));
}
}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
