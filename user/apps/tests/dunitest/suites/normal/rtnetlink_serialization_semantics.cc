#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <fcntl.h>
#include <linux/if_addr.h>
#include <linux/neighbour.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <net/if.h>
#include <sched.h>
#include <signal.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <unistd.h>

#include <array>
#include <atomic>
#include <cerrno>
#include <chrono>
#include <cstdint>
#include <cstring>
#include <string>
#include <thread>
#include <vector>

namespace {

constexpr int kWorkerIterations = 8;
constexpr int kExclusiveRaceRounds = 8;
constexpr int kExclusiveRaceWorkers = 4;
constexpr int kMaxRouteFillAttempts = 32;
constexpr int kCapacityNotReached = -2;

class FdGuard {
  public:
    explicit FdGuard(int fd = -1) : fd_(fd) {}
    FdGuard(const FdGuard&) = delete;
    FdGuard& operator=(const FdGuard&) = delete;
    ~FdGuard() {
        if (fd_ >= 0) close(fd_);
    }

    int Get() const { return fd_; }

  private:
    int fd_;
};

struct RouteSpec {
    uint32_t dst;
    uint8_t prefix_len;
    uint32_t ifindex;
    uint8_t table = RT_TABLE_MAIN;
    uint32_t source = 0;
    uint8_t source_prefix_len = 0;
};

struct ChildOutcome {
    bool timed_out;
    int exit_status;
    int stage;
};

uint32_t Ipv4(const std::string& text) {
    in_addr address{};
    if (inet_pton(AF_INET, text.c_str(), &address) != 1) return 0;
    return address.s_addr;
}

int OpenRouteSocket(uint32_t groups = 0) {
    int fd = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    if (fd < 0) return -1;

    timeval timeout{};
    timeout.tv_sec = 2;
    if (setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) < 0) {
        const int saved = errno;
        close(fd);
        errno = saved;
        return -1;
    }

    sockaddr_nl local{};
    local.nl_family = AF_NETLINK;
    local.nl_groups = groups;
    if (bind(fd, reinterpret_cast<sockaddr*>(&local), sizeof(local)) < 0) {
        const int saved = errno;
        close(fd);
        errno = saved;
        return -1;
    }
    return fd;
}

void AddAttr(std::vector<uint8_t>* request, uint16_t type, const void* data, size_t len) {
    auto* header = reinterpret_cast<nlmsghdr*>(request->data());
    const size_t offset = NLMSG_ALIGN(header->nlmsg_len);
    const size_t attr_len = RTA_LENGTH(len);
    const size_t end = offset + RTA_ALIGN(attr_len);
    request->resize(end, 0);

    header = reinterpret_cast<nlmsghdr*>(request->data());
    auto* attr = reinterpret_cast<rtattr*>(request->data() + offset);
    attr->rta_type = type;
    attr->rta_len = attr_len;
    std::memcpy(RTA_DATA(attr), data, len);
    header->nlmsg_len = end;
}

template <typename Body>
std::vector<uint8_t> NewRequest(uint16_t type, uint16_t flags, uint32_t seq) {
    std::vector<uint8_t> request(NLMSG_SPACE(sizeof(Body)), 0);
    auto* header = reinterpret_cast<nlmsghdr*>(request.data());
    header->nlmsg_len = NLMSG_LENGTH(sizeof(Body));
    header->nlmsg_type = type;
    header->nlmsg_flags = flags;
    header->nlmsg_seq = seq;
    request.resize(header->nlmsg_len);
    return request;
}

int RecvAck(int fd, uint32_t seq) {
    std::array<uint8_t, 8192> buffer{};
    for (;;) {
        const ssize_t received = recv(fd, buffer.data(), buffer.size(), 0);
        if (received < 0) return errno;

        int remaining = static_cast<int>(received);
        for (auto* header = reinterpret_cast<nlmsghdr*>(buffer.data());
             NLMSG_OK(header, remaining); header = NLMSG_NEXT(header, remaining)) {
            if (header->nlmsg_seq != seq || header->nlmsg_type != NLMSG_ERROR) continue;
            const auto* error = reinterpret_cast<const nlmsgerr*>(NLMSG_DATA(header));
            return error->error == 0 ? 0 : -error->error;
        }
    }
}

int SendAndRecvAck(int fd, const std::vector<uint8_t>& request) {
    if (send(fd, request.data(), request.size(), 0) != static_cast<ssize_t>(request.size())) {
        return errno;
    }
    return RecvAck(fd, reinterpret_cast<const nlmsghdr*>(request.data())->nlmsg_seq);
}

int SetLinkUp(int fd, uint32_t ifindex, uint32_t seq) {
    auto request = NewRequest<ifinfomsg>(RTM_SETLINK, NLM_F_REQUEST | NLM_F_ACK, seq);
    auto* link = reinterpret_cast<ifinfomsg*>(NLMSG_DATA(request.data()));
    link->ifi_family = AF_UNSPEC;
    link->ifi_index = static_cast<int>(ifindex);
    link->ifi_flags = IFF_UP;
    link->ifi_change = IFF_UP;
    return SendAndRecvAck(fd, request);
}

int RenameLink(int fd, uint32_t ifindex, const char* name, uint32_t seq) {
    auto request = NewRequest<ifinfomsg>(RTM_SETLINK, NLM_F_REQUEST | NLM_F_ACK, seq);
    auto* link = reinterpret_cast<ifinfomsg*>(NLMSG_DATA(request.data()));
    link->ifi_family = AF_UNSPEC;
    link->ifi_index = static_cast<int>(ifindex);
    AddAttr(&request, IFLA_IFNAME, name, std::strlen(name) + 1);
    return SendAndRecvAck(fd, request);
}

int ChangeAddress(int fd, uint16_t type, uint16_t flags, uint32_t ifindex, uint32_t address,
                  uint8_t prefix_len, uint32_t seq) {
    auto request = NewRequest<ifaddrmsg>(type, flags, seq);
    auto* addr = reinterpret_cast<ifaddrmsg*>(NLMSG_DATA(request.data()));
    addr->ifa_family = AF_INET;
    addr->ifa_prefixlen = prefix_len;
    addr->ifa_scope = RT_SCOPE_HOST;
    addr->ifa_index = ifindex;
    AddAttr(&request, IFA_LOCAL, &address, sizeof(address));
    AddAttr(&request, IFA_ADDRESS, &address, sizeof(address));
    return SendAndRecvAck(fd, request);
}

int ChangeRoute(int fd, uint16_t type, uint16_t flags, const RouteSpec& route, uint32_t seq) {
    auto request = NewRequest<rtmsg>(type, flags, seq);
    auto* message = reinterpret_cast<rtmsg*>(NLMSG_DATA(request.data()));
    message->rtm_family = AF_INET;
    message->rtm_dst_len = route.prefix_len;
    message->rtm_src_len = route.source_prefix_len;
    message->rtm_table = route.table;
    message->rtm_protocol = RTPROT_STATIC;
    message->rtm_scope = RT_SCOPE_LINK;
    message->rtm_type = RTN_UNICAST;
    AddAttr(&request, RTA_DST, &route.dst, sizeof(route.dst));
    AddAttr(&request, RTA_OIF, &route.ifindex, sizeof(route.ifindex));
    if (route.source_prefix_len != 0) {
        AddAttr(&request, RTA_SRC, &route.source, sizeof(route.source));
    }
    return SendAndRecvAck(fd, request);
}

int ReplaceNeighbour(int fd, uint32_t ifindex, uint32_t destination, uint32_t seq) {
    auto request = NewRequest<ndmsg>(
        RTM_NEWNEIGH, NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_REPLACE, seq);
    auto* neighbour = reinterpret_cast<ndmsg*>(NLMSG_DATA(request.data()));
    neighbour->ndm_family = AF_INET;
    neighbour->ndm_ifindex = static_cast<int>(ifindex);
    neighbour->ndm_state = NUD_PERMANENT;
    const uint8_t mac[6] = {0x02, 0x00, 0x00, 0x00, 0x02, 0x23};
    AddAttr(&request, NDA_DST, &destination, sizeof(destination));
    AddAttr(&request, NDA_LLADDR, mac, sizeof(mac));
    return SendAndRecvAck(fd, request);
}

bool MessageContainsRoute(const nlmsghdr* header, const RouteSpec& route) {
    if (header->nlmsg_type != RTM_NEWROUTE && header->nlmsg_type != RTM_DELROUTE) return false;
    const auto* message = reinterpret_cast<const rtmsg*>(NLMSG_DATA(header));
    if (message->rtm_family != AF_INET || message->rtm_dst_len != route.prefix_len ||
        message->rtm_table != route.table || message->rtm_src_len != route.source_prefix_len) {
        return false;
    }

    uint32_t destination = 0;
    uint32_t ifindex = 0;
    uint32_t source = 0;
    int attr_len = RTM_PAYLOAD(header);
    for (auto* attr = RTM_RTA(message); RTA_OK(attr, attr_len);
         attr = RTA_NEXT(attr, attr_len)) {
        if (attr->rta_type == RTA_DST && RTA_PAYLOAD(attr) >= sizeof(destination)) {
            std::memcpy(&destination, RTA_DATA(attr), sizeof(destination));
        } else if (attr->rta_type == RTA_OIF && RTA_PAYLOAD(attr) >= sizeof(ifindex)) {
            std::memcpy(&ifindex, RTA_DATA(attr), sizeof(ifindex));
        } else if (attr->rta_type == RTA_SRC && RTA_PAYLOAD(attr) >= sizeof(source)) {
            std::memcpy(&source, RTA_DATA(attr), sizeof(source));
        }
    }
    return destination == route.dst && ifindex == route.ifindex && source == route.source;
}

int DumpRoutes(int fd, uint32_t seq, const RouteSpec* probe = nullptr,
               int* match_count = nullptr) {
    auto request = NewRequest<rtmsg>(RTM_GETROUTE, NLM_F_REQUEST | NLM_F_DUMP, seq);
    auto* message = reinterpret_cast<rtmsg*>(NLMSG_DATA(request.data()));
    message->rtm_family = AF_INET;
    if (send(fd, request.data(), request.size(), 0) != static_cast<ssize_t>(request.size())) {
        return errno;
    }

    std::array<uint8_t, 16384> buffer{};
    for (;;) {
        const ssize_t received = recv(fd, buffer.data(), buffer.size(), 0);
        if (received < 0) return errno;
        int remaining = static_cast<int>(received);
        for (auto* header = reinterpret_cast<nlmsghdr*>(buffer.data());
             NLMSG_OK(header, remaining); header = NLMSG_NEXT(header, remaining)) {
            if (header->nlmsg_seq != seq) continue;
            if (probe != nullptr && MessageContainsRoute(header, *probe)) {
                if (match_count == nullptr) return EEXIST;
                ++(*match_count);
            }
            if (header->nlmsg_type == NLMSG_DONE) return 0;
            if (header->nlmsg_type == NLMSG_ERROR) {
                const auto* error = reinterpret_cast<const nlmsgerr*>(NLMSG_DATA(header));
                return error->error == 0 ? 0 : -error->error;
            }
        }
    }
}

int DumpAddresses(int fd, uint32_t seq) {
    auto request = NewRequest<ifaddrmsg>(RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, seq);
    auto* message = reinterpret_cast<ifaddrmsg*>(NLMSG_DATA(request.data()));
    message->ifa_family = AF_INET;
    if (send(fd, request.data(), request.size(), 0) != static_cast<ssize_t>(request.size())) {
        return errno;
    }

    std::array<uint8_t, 16384> buffer{};
    for (;;) {
        const ssize_t received = recv(fd, buffer.data(), buffer.size(), 0);
        if (received < 0) return errno;
        int remaining = static_cast<int>(received);
        for (auto* header = reinterpret_cast<nlmsghdr*>(buffer.data());
             NLMSG_OK(header, remaining); header = NLMSG_NEXT(header, remaining)) {
            if (header->nlmsg_seq != seq) continue;
            if (header->nlmsg_type == NLMSG_DONE) return 0;
            if (header->nlmsg_type == NLMSG_ERROR) {
                const auto* error = reinterpret_cast<const nlmsgerr*>(NLMSG_DATA(header));
                return error->error == 0 ? 0 : -error->error;
            }
        }
    }
}

void RecordFailure(std::atomic<int>* first_failure, int stage, int error) {
    const int encoded = stage * 1000 + error;
    int expected = 0;
    first_failure->compare_exchange_strong(expected, encoded);
}

void WaitForStart(std::atomic<int>* ready, std::atomic<bool>* start) {
    ready->fetch_add(1, std::memory_order_release);
    while (!start->load(std::memory_order_acquire)) std::this_thread::yield();
}

int RunExclusiveRouteRaces(int cleanup_fd, uint32_t ifindex) {
    const RouteSpec route{Ipv4("198.22.0.0"), 24, ifindex};
    uint32_t cleanup_seq = 1000;

    for (int round = 0; round < kExclusiveRaceRounds; ++round) {
        (void)ChangeRoute(cleanup_fd, RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, route,
                          cleanup_seq++);

        std::atomic<int> ready{0};
        std::atomic<bool> start{false};
        std::array<int, kExclusiveRaceWorkers> results{};
        std::vector<std::thread> workers;
        workers.reserve(kExclusiveRaceWorkers);
        for (int worker = 0; worker < kExclusiveRaceWorkers; ++worker) {
            workers.emplace_back([&, worker] {
                FdGuard fd(OpenRouteSocket());
                if (fd.Get() < 0) {
                    results[worker] = -errno;
                    ready.fetch_add(1, std::memory_order_release);
                    return;
                }
                WaitForStart(&ready, &start);
                results[worker] = ChangeRoute(
                    fd.Get(), RTM_NEWROUTE,
                    NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, route,
                    1100 + round * kExclusiveRaceWorkers + worker);
            });
        }

        while (ready.load(std::memory_order_acquire) != kExclusiveRaceWorkers) {
            std::this_thread::yield();
        }
        start.store(true, std::memory_order_release);
        for (auto& worker : workers) worker.join();

        int successes = 0;
        int duplicates = 0;
        for (const int result : results) {
            if (result == 0) {
                ++successes;
            } else if (result == EEXIST) {
                ++duplicates;
            } else {
                return 6000 + (result < 0 ? -result : result);
            }
        }
        if (successes != 1 || duplicates != kExclusiveRaceWorkers - 1) return 7000;

        int match_count = 0;
        if (DumpRoutes(cleanup_fd, cleanup_seq++, &route, &match_count) != 0 ||
            match_count != 1) {
            return 8000 + match_count;
        }
        if (ChangeRoute(cleanup_fd, RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, route,
                        cleanup_seq++) != 0) {
            return 9000;
        }
    }
    return 0;
}

int RunConcurrentMutations() {
    if (unshare(CLONE_NEWUSER | CLONE_NEWNET) != 0) return 1000 + errno;
    FdGuard setup(OpenRouteSocket());
    if (setup.Get() < 0) return 2000 + errno;
    const uint32_t ifindex = if_nametoindex("lo");
    if (ifindex == 0) return 3000 + errno;
    if (const int error = SetLinkUp(setup.Get(), ifindex, 1); error != 0) return 4000 + error;

    const uint32_t address = Ipv4("198.19.0.1");
    const RouteSpec route{Ipv4("198.20.0.0"), 24, ifindex};
    const uint32_t neighbour = Ipv4("198.21.0.1");
    std::atomic<int> ready{0};
    std::atomic<bool> start{false};
    std::atomic<int> first_failure{0};

    std::thread link_worker([&] {
        WaitForStart(&ready, &start);
        FdGuard fd(OpenRouteSocket());
        if (fd.Get() < 0) {
            RecordFailure(&first_failure, 10, errno);
            return;
        }
        for (int i = 0; i < kWorkerIterations; ++i) {
            int error = RenameLink(fd.Get(), ifindex, "pr02lo", 100 + i * 2);
            if (error == 0) error = RenameLink(fd.Get(), ifindex, "lo", 101 + i * 2);
            if (error != 0) {
                RecordFailure(&first_failure, 11, error);
                return;
            }
        }
    });

    std::thread address_worker([&] {
        WaitForStart(&ready, &start);
        FdGuard fd(OpenRouteSocket());
        if (fd.Get() < 0) {
            RecordFailure(&first_failure, 20, errno);
            return;
        }
        for (int i = 0; i < kWorkerIterations; ++i) {
            int error = ChangeAddress(fd.Get(), RTM_NEWADDR,
                                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                      ifindex, address, 32, 200 + i * 2);
            if (error == 0) {
                error = ChangeAddress(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, ifindex,
                                      address, 32, 201 + i * 2);
            }
            if (error != 0) {
                RecordFailure(&first_failure, 21, error);
                return;
            }
        }
    });

    std::thread route_worker([&] {
        WaitForStart(&ready, &start);
        FdGuard fd(OpenRouteSocket());
        if (fd.Get() < 0) {
            RecordFailure(&first_failure, 30, errno);
            return;
        }
        for (int i = 0; i < kWorkerIterations; ++i) {
            int error = ChangeRoute(fd.Get(), RTM_NEWROUTE,
                                    NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                    route, 300 + i * 2);
            if (error == 0) {
                error = ChangeRoute(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, route,
                                    301 + i * 2);
            }
            if (error != 0) {
                RecordFailure(&first_failure, 31, error);
                return;
            }
        }
    });

    std::thread neighbour_worker([&] {
        WaitForStart(&ready, &start);
        FdGuard fd(OpenRouteSocket());
        if (fd.Get() < 0) {
            RecordFailure(&first_failure, 40, errno);
            return;
        }
        for (int i = 0; i < kWorkerIterations; ++i) {
            const int error = ReplaceNeighbour(fd.Get(), ifindex, neighbour, 400 + i);
            if (error != 0) {
                RecordFailure(&first_failure, 41, error);
                return;
            }
        }
    });

    std::thread dump_worker([&] {
        WaitForStart(&ready, &start);
        FdGuard fd(OpenRouteSocket());
        if (fd.Get() < 0) {
            RecordFailure(&first_failure, 50, errno);
            return;
        }
        for (int i = 0; i < kWorkerIterations; ++i) {
            int error = DumpRoutes(fd.Get(), 500 + i * 2);
            if (error == 0) error = DumpAddresses(fd.Get(), 501 + i * 2);
            if (error != 0) {
                RecordFailure(&first_failure, 51, error);
                return;
            }
        }
    });

    while (ready.load(std::memory_order_acquire) != 5) std::this_thread::yield();
    start.store(true, std::memory_order_release);
    link_worker.join();
    address_worker.join();
    route_worker.join();
    neighbour_worker.join();
    dump_worker.join();

    (void)RenameLink(setup.Get(), ifindex, "lo", 900);
    (void)ChangeAddress(setup.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, ifindex, address, 32,
                        901);
    (void)ChangeRoute(setup.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, route, 902);
    if (const int failure = first_failure.load(); failure != 0) return failure;
    return RunExclusiveRouteRaces(setup.Get(), ifindex);
}

void DrainNotifications(int fd) {
    std::array<uint8_t, 16384> buffer{};
    while (recv(fd, buffer.data(), buffer.size(), MSG_DONTWAIT) > 0) {
    }
}

int HasRouteNotification(int fd, const RouteSpec& route) {
    std::array<uint8_t, 16384> buffer{};
    for (;;) {
        const ssize_t received = recv(fd, buffer.data(), buffer.size(), MSG_DONTWAIT);
        if (received < 0) {
            return errno == EAGAIN || errno == EWOULDBLOCK ? 0 : -errno;
        }
        if (received == 0) return 0;
        int remaining = static_cast<int>(received);
        for (auto* header = reinterpret_cast<nlmsghdr*>(buffer.data());
             NLMSG_OK(header, remaining); header = NLMSG_NEXT(header, remaining)) {
            if (header->nlmsg_type == RTM_NEWROUTE && MessageContainsRoute(header, route)) {
                return 1;
            }
        }
    }
}

int RunFailedRouteAtomicity() {
    if (unshare(CLONE_NEWUSER | CLONE_NEWNET) != 0) return 1000 + errno;
    FdGuard request_fd(OpenRouteSocket());
    FdGuard notify_fd(OpenRouteSocket(RTMGRP_IPV4_ROUTE));
    if (request_fd.Get() < 0 || notify_fd.Get() < 0) return 2000 + errno;
    const uint32_t ifindex = if_nametoindex("lo");
    if (ifindex == 0) return 3000 + errno;
    if (const int error = SetLinkUp(request_fd.Get(), ifindex, 1); error != 0) {
        return 4000 + error;
    }
    utsname host{};
    if (uname(&host) != 0) return 4500 + errno;
    if (std::strstr(host.release, "dragonos") == nullptr) return kCapacityNotReached;

    const RouteSpec preserved{Ipv4("198.29.0.0"), 24, ifindex};
    RouteSpec shadow = preserved;
    shadow.source = Ipv4("198.28.0.1");
    shadow.source_prefix_len = 32;
    uint32_t seq = 100;
    if (ChangeRoute(request_fd.Get(), RTM_NEWROUTE,
                    NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, preserved,
                    seq++) != 0 ||
        ChangeRoute(request_fd.Get(), RTM_NEWROUTE,
                    NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, shadow, seq++) != 0 ||
        ChangeRoute(request_fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, shadow, seq++) !=
            0) {
        return 5000;
    }
    // DragonOS currently projects route keys with the same destination onto
    // one smoltcp entry. Removing the source-specific shadow key leaves the
    // source-agnostic control record present while removing that projection,
    // exposing the historical replace rollback failure through public
    // rtnetlink operations.
    DrainNotifications(notify_fd.Get());

    std::vector<RouteSpec> added;
    int failure_error = 0;
    for (int i = 0; i < kMaxRouteFillAttempts; ++i) {
        RouteSpec candidate{Ipv4("198.30." + std::to_string(i) + ".0"), 24, ifindex};
        const int error = ChangeRoute(request_fd.Get(), RTM_NEWROUTE,
                                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                      candidate, seq++);
        if (error == 0) {
            added.push_back(candidate);
            DrainNotifications(notify_fd.Get());
            continue;
        }
        if (error == ENOSPC) {
            failure_error = error;
            break;
        }
        return 5000 + error;
    }

    if (failure_error != ENOSPC) {
        for (const auto& route : added) {
            (void)ChangeRoute(request_fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, route,
                              seq++);
        }
        (void)ChangeRoute(request_fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, preserved,
                          seq++);
        return kCapacityNotReached;
    }

    if (added.empty()) return 6000;

    // A full table must still permit an in-place replace of an existing
    // projection; this exercises the non-fallible first-match branch.
    if (ChangeRoute(request_fd.Get(), RTM_NEWROUTE,
                    NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_REPLACE, added.front(),
                    seq++) != 0) {
        return 7000;
    }
    int existing_count = 0;
    if (DumpRoutes(request_fd.Get(), seq++, &added.front(), &existing_count) != 0 ||
        existing_count != 1) {
        return 8000 + existing_count;
    }
    DrainNotifications(notify_fd.Get());

    // Replacing the preserved control record needs a new projection and must
    // fail atomically when the data-plane table is full.
    if (ChangeRoute(request_fd.Get(), RTM_NEWROUTE,
                    NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_REPLACE, preserved,
                    seq++) != ENOSPC) {
        return 9000;
    }
    int preserved_count = 0;
    if (DumpRoutes(request_fd.Get(), seq++, &preserved, &preserved_count) != 0 ||
        preserved_count != 1) {
        return 10000 + preserved_count;
    }
    const int notification = HasRouteNotification(notify_fd.Get(), preserved);
    if (notification > 0) return 11000;
    if (notification < 0) return 11000 - notification;

    FdGuard udp(socket(AF_INET, SOCK_DGRAM, 0));
    if (udp.Get() < 0) return 8000 + errno;
    sockaddr_in destination{};
    destination.sin_family = AF_INET;
    destination.sin_port = htons(9);
    destination.sin_addr.s_addr = preserved.dst | htonl(1);
    const char payload = 'x';
    errno = 0;
    if (sendto(udp.Get(), &payload, sizeof(payload), 0,
               reinterpret_cast<sockaddr*>(&destination), sizeof(destination)) >= 0 ||
        errno != ENETUNREACH) {
        return 12000 + errno;
    }

    for (const auto& route : added) {
        const int error =
            ChangeRoute(request_fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, route, seq++);
        if (error != 0) return 13000 + error;
    }
    if (ChangeRoute(request_fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, preserved,
                    seq++) != 0) {
        return 14000;
    }
    return 0;
}

template <typename Function>
ChildOutcome RunWithWatchdog(Function function) {
    int result_pipe[2];
    if (pipe(result_pipe) != 0) return {false, -1, 11000 + errno};

    const pid_t child = fork();
    if (child < 0) {
        const int saved = errno;
        close(result_pipe[0]);
        close(result_pipe[1]);
        return {false, -1, 12000 + saved};
    }
    if (child == 0) {
        close(result_pipe[0]);
        const int stage = function();
        const ssize_t ignored = write(result_pipe[1], &stage, sizeof(stage));
        (void)ignored;
        close(result_pipe[1]);
        _exit(stage == 0 || stage == kCapacityNotReached ? 0 : 1);
    }

    close(result_pipe[1]);
    int status = 0;
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(15);
    for (;;) {
        const pid_t waited = waitpid(child, &status, WNOHANG);
        if (waited == child) break;
        if (waited < 0) {
            close(result_pipe[0]);
            return {false, -1, 13000 + errno};
        }
        if (std::chrono::steady_clock::now() >= deadline) {
            (void)kill(child, SIGKILL);
            (void)waitpid(child, &status, 0);
            close(result_pipe[0]);
            return {true, status, 0};
        }
        usleep(10 * 1000);
    }

    int stage = -1;
    const ssize_t bytes = read(result_pipe[0], &stage, sizeof(stage));
    close(result_pipe[0]);
    if (bytes != static_cast<ssize_t>(sizeof(stage))) stage = 14000;
    return {false, status, stage};
}

TEST(RtnetlinkSerializationSemantics, ConcurrentMutationsAndDumpsComplete) {
    const ChildOutcome outcome = RunWithWatchdog(RunConcurrentMutations);
    ASSERT_FALSE(outcome.timed_out) << "rtnetlink workers deadlocked in send/handler";
    ASSERT_TRUE(WIFEXITED(outcome.exit_status));
    EXPECT_EQ(outcome.stage, 0) << "encoded stage/error=" << outcome.stage;
    EXPECT_EQ(WEXITSTATUS(outcome.exit_status), 0);
}

TEST(RtnetlinkSerializationSemantics, FailedRouteHasNoCommitOrSuccessNotification) {
    const ChildOutcome outcome = RunWithWatchdog(RunFailedRouteAtomicity);
    ASSERT_FALSE(outcome.timed_out) << "rtnetlink failure path deadlocked";
    ASSERT_TRUE(WIFEXITED(outcome.exit_status));
    if (outcome.stage == kCapacityNotReached) {
        utsname name{};
        ASSERT_EQ(uname(&name), 0);
        if (std::strstr(name.release, "dragonos") == nullptr) {
            GTEST_SKIP() << "Linux FIB has no DragonOS smoltcp fixed-capacity ENOSPC boundary";
        }
        FAIL() << "DragonOS route table did not reach ENOSPC within the bounded fill loop";
    }
    EXPECT_EQ(outcome.stage, 0) << "encoded stage/error=" << outcome.stage;
    EXPECT_EQ(WEXITSTATUS(outcome.exit_status), 0);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
