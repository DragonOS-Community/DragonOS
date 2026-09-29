#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <fcntl.h>
#include <linux/if_ether.h>
#include <linux/if_packet.h>
#include <linux/netlink.h>
#include <linux/neighbour.h>
#include <linux/rtnetlink.h>
#include <linux/veth.h>
#include <net/if.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <unistd.h>

#include <array>
#include <algorithm>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <cstdint>
#include <cstring>
#include <vector>

namespace {

class Fd {
  public:
    explicit Fd(int fd = -1) : fd_(fd) {}
    ~Fd() { if (fd_ >= 0) close(fd_); }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int Get() const { return fd_; }
  private:
    int fd_;
};

void Attr(std::vector<uint8_t>* request, uint16_t type, const void* data, size_t size) {
    auto* header = reinterpret_cast<nlmsghdr*>(request->data());
    const size_t offset = NLMSG_ALIGN(header->nlmsg_len);
    request->resize(offset + RTA_ALIGN(RTA_LENGTH(size)), 0);
    auto* attr = reinterpret_cast<rtattr*>(request->data() + offset);
    attr->rta_type = type;
    attr->rta_len = RTA_LENGTH(size);
    if (size) std::memcpy(RTA_DATA(attr), data, size);
    reinterpret_cast<nlmsghdr*>(request->data())->nlmsg_len = request->size();
}

std::vector<uint8_t> Request(uint16_t type, uint16_t flags, uint32_t seq, size_t body) {
    std::vector<uint8_t> request(NLMSG_LENGTH(body), 0);
    auto* header = reinterpret_cast<nlmsghdr*>(request.data());
    header->nlmsg_len = request.size();
    header->nlmsg_type = type;
    header->nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK | flags;
    header->nlmsg_seq = seq;
    return request;
}

int Ack(int fd, const std::vector<uint8_t>& request) {
    const auto seq = reinterpret_cast<const nlmsghdr*>(request.data())->nlmsg_seq;
    if (send(fd, request.data(), request.size(), 0) != static_cast<ssize_t>(request.size()))
        return errno;
    std::array<uint8_t, 4096> response{};
    for (;;) {
        ssize_t size = recv(fd, response.data(), response.size(), 0);
        if (size < 0) return errno;
        int left = static_cast<int>(size);
        for (auto* header = reinterpret_cast<nlmsghdr*>(response.data());
             NLMSG_OK(header, left); header = NLMSG_NEXT(header, left)) {
            if (header->nlmsg_seq != seq || header->nlmsg_type != NLMSG_ERROR) continue;
            auto* error = reinterpret_cast<nlmsgerr*>(NLMSG_DATA(header));
            return error->error == 0 ? 0 : -error->error;
        }
    }
}

int CreateVeth(int fd, const char* router, const char* peer, uint32_t seq) {
    std::vector<uint8_t> peer_body(
        sizeof(ifinfomsg) + RTA_ALIGN(RTA_LENGTH(std::strlen(peer) + 1)), 0);
    // Nested VETH_INFO_PEER starts with an ifinfomsg, followed by IFLA_IFNAME.
    const size_t offset = sizeof(ifinfomsg);
    auto* name = reinterpret_cast<rtattr*>(peer_body.data() + offset);
    name->rta_type = IFLA_IFNAME;
    name->rta_len = RTA_LENGTH(std::strlen(peer) + 1);
    std::memcpy(RTA_DATA(name), peer, std::strlen(peer) + 1);
    std::vector<uint8_t> data(RTA_ALIGN(RTA_LENGTH(peer_body.size())), 0);
    auto* peer_attr = reinterpret_cast<rtattr*>(data.data());
    peer_attr->rta_type = VETH_INFO_PEER;
    peer_attr->rta_len = RTA_LENGTH(peer_body.size());
    std::memcpy(RTA_DATA(peer_attr), peer_body.data(), peer_body.size());
    const char kind[] = "veth";
    std::vector<uint8_t> info(RTA_ALIGN(RTA_LENGTH(sizeof(kind))) +
                              RTA_ALIGN(RTA_LENGTH(data.size())), 0);
    auto* kind_attr = reinterpret_cast<rtattr*>(info.data());
    kind_attr->rta_type = IFLA_INFO_KIND;
    kind_attr->rta_len = RTA_LENGTH(sizeof(kind));
    std::memcpy(RTA_DATA(kind_attr), kind, sizeof(kind));
    auto* data_attr = reinterpret_cast<rtattr*>(info.data() + RTA_ALIGN(kind_attr->rta_len));
    data_attr->rta_type = IFLA_INFO_DATA;
    data_attr->rta_len = RTA_LENGTH(data.size());
    std::memcpy(RTA_DATA(data_attr), data.data(), data.size());
    auto request = Request(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, seq, sizeof(ifinfomsg));
    Attr(&request, IFLA_IFNAME, router, std::strlen(router) + 1);
    Attr(&request, IFLA_LINKINFO, info.data(), info.size());
    return Ack(fd, request);
}

int SetLink(int fd, unsigned index, uint32_t seq, uint32_t mtu = 0) {
    auto request = Request(RTM_SETLINK, 0, seq, sizeof(ifinfomsg));
    auto* info = reinterpret_cast<ifinfomsg*>(NLMSG_DATA(request.data()));
    info->ifi_index = index;
    info->ifi_flags = IFF_UP;
    info->ifi_change = IFF_UP;
    if (mtu) Attr(&request, IFLA_MTU, &mtu, sizeof(mtu));
    return Ack(fd, request);
}

int Address(int fd, unsigned index, uint32_t seq, const char* text) {
    auto request = Request(RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL, seq, sizeof(ifaddrmsg));
    auto* info = reinterpret_cast<ifaddrmsg*>(NLMSG_DATA(request.data()));
    info->ifa_family = AF_INET;
    info->ifa_prefixlen = 24;
    info->ifa_index = index;
    in_addr address{};
    inet_pton(AF_INET, text, &address);
    Attr(&request, IFA_LOCAL, &address, sizeof(address));
    Attr(&request, IFA_ADDRESS, &address, sizeof(address));
    return Ack(fd, request);
}

int Neighbor(int fd, unsigned index, uint32_t seq, const char* text,
             const std::array<uint8_t, 6>& mac) {
    auto request = Request(RTM_NEWNEIGH, NLM_F_CREATE | NLM_F_REPLACE, seq, sizeof(ndmsg));
    auto* info = reinterpret_cast<ndmsg*>(NLMSG_DATA(request.data()));
    info->ndm_family = AF_INET;
    info->ndm_ifindex = index;
    info->ndm_state = NUD_PERMANENT;
    in_addr address{};
    inet_pton(AF_INET, text, &address);
    Attr(&request, NDA_DST, &address, sizeof(address));
    Attr(&request, NDA_LLADDR, mac.data(), mac.size());
    return Ack(fd, request);
}

int Mac(const char* name, std::array<uint8_t, 6>* mac) {
    Fd fd(socket(AF_INET, SOCK_DGRAM, 0));
    if (fd.Get() < 0) return errno;
    ifreq request{};
    std::strncpy(request.ifr_name, name, IFNAMSIZ - 1);
    if (ioctl(fd.Get(), SIOCGIFHWADDR, &request) < 0) return errno;
    std::memcpy(mac->data(), request.ifr_hwaddr.sa_data, mac->size());
    return 0;
}

uint16_t Checksum(const uint8_t* bytes, size_t size) {
    uint32_t sum = 0;
    for (size_t i = 0; i < size; i += 2)
        sum += static_cast<uint16_t>(bytes[i]) << 8 | (i + 1 < size ? bytes[i + 1] : 0);
    while (sum >> 16) sum = (sum & 0xffff) + (sum >> 16);
    return ~sum;
}

int PacketSocket(unsigned index, uint16_t protocol = ETH_P_IP) {
    int fd = socket(AF_PACKET, SOCK_DGRAM, htons(protocol));
    if (fd < 0) return -1;
    sockaddr_ll address{};
    address.sll_family = AF_PACKET;
    address.sll_protocol = htons(protocol);
    address.sll_ifindex = index;
    if (bind(fd, reinterpret_cast<sockaddr*>(&address), sizeof(address)) < 0) {
        const int error = errno;
        close(fd);
        errno = error;
        return -1;
    }
    return fd;
}

int Address6(int fd, unsigned index, uint32_t seq, const char* text) {
    auto request = Request(RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL, seq, sizeof(ifaddrmsg));
    auto* info = reinterpret_cast<ifaddrmsg*>(NLMSG_DATA(request.data()));
    info->ifa_family = AF_INET6;
    info->ifa_prefixlen = 64;
    info->ifa_index = index;
    info->ifa_flags = IFA_F_NODAD;
    in6_addr address{};
    inet_pton(AF_INET6, text, &address);
    Attr(&request, IFA_ADDRESS, &address, sizeof(address));
    return Ack(fd, request);
}

int Route6(int fd, unsigned index, uint32_t seq, const char* text) {
    auto request = Request(RTM_NEWROUTE, NLM_F_CREATE | NLM_F_EXCL, seq, sizeof(rtmsg));
    auto* route = reinterpret_cast<rtmsg*>(NLMSG_DATA(request.data()));
    route->rtm_family = AF_INET6;
    route->rtm_dst_len = 128;
    route->rtm_table = RT_TABLE_MAIN;
    route->rtm_protocol = RTPROT_STATIC;
    route->rtm_scope = RT_SCOPE_LINK;
    route->rtm_type = RTN_UNICAST;
    in6_addr destination{};
    inet_pton(AF_INET6, text, &destination);
    Attr(&request, RTA_DST, &destination, sizeof(destination));
    Attr(&request, RTA_OIF, &index, sizeof(index));
    return Ack(fd, request);
}

int Neighbor6(int fd, unsigned index, uint32_t seq, const char* text,
              const std::array<uint8_t, 6>& mac) {
    auto request = Request(RTM_NEWNEIGH, NLM_F_CREATE | NLM_F_REPLACE, seq, sizeof(ndmsg));
    auto* info = reinterpret_cast<ndmsg*>(NLMSG_DATA(request.data()));
    info->ndm_family = AF_INET6;
    info->ndm_ifindex = index;
    info->ndm_state = NUD_PERMANENT;
    in6_addr address{};
    inet_pton(AF_INET6, text, &address);
    Attr(&request, NDA_DST, &address, sizeof(address));
    Attr(&request, NDA_LLADDR, mac.data(), mac.size());
    return Ack(fd, request);
}

int Inject6(int fd, unsigned index, const std::array<uint8_t, 6>& router_mac,
            uint16_t source_port = 0xd037) {
    std::vector<uint8_t> ip(1448, 0x5a);
    std::fill_n(ip.begin(), 48, 0);
    ip[0] = 0x60;
    ip[4] = 0x05;
    ip[5] = 0x80;
    ip[6] = IPPROTO_UDP;
    ip[7] = 64;
    inet_pton(AF_INET6, "fd37:1::2", &ip[8]);
    inet_pton(AF_INET6, "fd37:2::2", &ip[24]);
    ip[40] = source_port >> 8;
    ip[41] = source_port;
    ip[42] = 0xd0;
    ip[43] = 0x38;
    ip[44] = 0x05;
    ip[45] = 0x80;
    uint32_t sum = IPPROTO_UDP + 1408;
    for (size_t i = 8; i < 40; i += 2) sum += unsigned(ip[i]) << 8 | ip[i + 1];
    for (size_t i = 40; i < ip.size(); i += 2) sum += unsigned(ip[i]) << 8 | ip[i + 1];
    while (sum >> 16) sum = (sum & 0xffff) + (sum >> 16);
    const uint16_t checksum = ~sum;
    ip[46] = checksum >> 8;
    ip[47] = checksum;
    sockaddr_ll address{};
    address.sll_family = AF_PACKET;
    address.sll_protocol = htons(ETH_P_IPV6);
    address.sll_ifindex = index;
    address.sll_halen = ETH_ALEN;
    std::memcpy(address.sll_addr, router_mac.data(), router_mac.size());
    return sendto(fd, ip.data(), ip.size(), 0, reinterpret_cast<sockaddr*>(&address),
                  sizeof(address)) == static_cast<ssize_t>(ip.size()) ? 0 : errno;
}

int Inject6Fragments(int fd, unsigned index, const std::array<uint8_t, 6>& router_mac) {
    std::vector<uint8_t> udp(2000, 0x5a);
    udp[0] = 0xd0;
    udp[1] = 0x37;
    udp[2] = 0xd0;
    udp[3] = 0x38;
    udp[4] = 0x07;
    udp[5] = 0xd0;
    udp[6] = udp[7] = 0;
    in6_addr source{}, destination{};
    inet_pton(AF_INET6, "fd37:1::2", &source);
    inet_pton(AF_INET6, "fd37:2::2", &destination);
    uint32_t sum = IPPROTO_UDP + udp.size();
    for (const auto* addr : {&source, &destination}) {
        const auto* data = reinterpret_cast<const uint8_t*>(addr);
        for (size_t i = 0; i < 16; i += 2) sum += unsigned(data[i]) << 8 | data[i + 1];
    }
    for (size_t i = 0; i < udp.size(); i += 2) sum += unsigned(udp[i]) << 8 | udp[i + 1];
    while (sum >> 16) sum = (sum & 0xffff) + (sum >> 16);
    const uint16_t checksum = ~sum;
    udp[6] = checksum >> 8;
    udp[7] = checksum;
    sockaddr_ll address{};
    address.sll_family = AF_PACKET;
    address.sll_protocol = htons(ETH_P_IPV6);
    address.sll_ifindex = index;
    address.sll_halen = ETH_ALEN;
    std::memcpy(address.sll_addr, router_mac.data(), router_mac.size());
    for (size_t offset : {size_t(0), size_t(696)}) {
        const size_t chunk = offset == 0 ? 696 : 1304;
        std::vector<uint8_t> fragment(56 + chunk, 0);
        fragment[0] = 0x60;
        const uint16_t payload = 16 + chunk;
        fragment[4] = payload >> 8;
        fragment[5] = payload;
        fragment[6] = 0;  // Hop-by-Hop before Fragment.
        fragment[7] = 64;
        std::memcpy(&fragment[8], &source, 16);
        std::memcpy(&fragment[24], &destination, 16);
        fragment[40] = 44;
        fragment[48] = IPPROTO_UDP;
        const uint16_t flags = static_cast<uint16_t>(offset) | (offset == 0 ? 1 : 0);
        fragment[50] = flags >> 8;
        fragment[51] = flags;
        fragment[52] = 0x37;
        fragment[53] = 0x00;
        fragment[54] = 0x00;
        fragment[55] = 0x06;
        std::copy_n(udp.begin() + offset, chunk, fragment.begin() + 56);
        if (sendto(fd, fragment.data(), fragment.size(), 0,
                   reinterpret_cast<sockaddr*>(&address), sizeof(address)) !=
            static_cast<ssize_t>(fragment.size())) return errno;
    }
    return 0;
}

int Capture6Fragments(int fd) {
    unsigned total_payload = 0;
    int fragments = 0;
    bool saw_offset_1304 = false;
    unsigned last_offset = 0;
    int last_length = 0;
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(2);
    while (std::chrono::steady_clock::now() < deadline) {
        pollfd pfd{fd, POLLIN, 0};
        if (poll(&pfd, 1, 100) <= 0) continue;
        std::array<uint8_t, 2048> bytes{};
        sockaddr_ll source{};
        socklen_t size = sizeof(source);
        ssize_t count = recvfrom(fd, bytes.data(), bytes.size(), 0,
                                 reinterpret_cast<sockaddr*>(&source), &size);
        if (count < 56 || source.sll_pkttype == PACKET_OUTGOING || bytes[0] >> 4 != 6 ||
            bytes[6] != 0 || bytes[40] != 44 || bytes[48] != IPPROTO_UDP ||
            bytes[7] != 63 || count > 1360) continue;
        const unsigned offset = (unsigned(bytes[50]) << 8 | bytes[51]) & 0xfff8;
        last_offset = offset;
        last_length = count;
        saw_offset_1304 |= offset == 1304;
        total_payload += static_cast<unsigned>(count) - 56;
        ++fragments;
        if (fragments >= 2 && total_payload == 2000 && saw_offset_1304) return 0;
    }
    std::fprintf(stderr, "IPv6 fragments: count=%d payload=%u last_offset=%u last_length=%d\n",
                 fragments, total_payload, last_offset, last_length);
    return 1;
}

int Capture6(int fd) {
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(2);
    while (std::chrono::steady_clock::now() < deadline) {
        pollfd pfd{fd, POLLIN, 0};
        if (poll(&pfd, 1, 100) <= 0) continue;
        std::array<uint8_t, 2048> bytes{};
        sockaddr_ll source{};
        socklen_t size = sizeof(source);
        ssize_t count = recvfrom(fd, bytes.data(), bytes.size(), 0,
                                 reinterpret_cast<sockaddr*>(&source), &size);
        if (count < 96 || source.sll_pkttype == PACKET_OUTGOING || bytes[0] >> 4 != 6)
            continue;
        if (bytes[6] != 58 || bytes[40] != 2 || bytes[41] != 0 ||
            bytes[44] != 0 || bytes[45] != 0 || bytes[46] != 5 || bytes[47] != 0 ||
            bytes[48] >> 4 != 6 || bytes[55] != 64) continue;
        return 0;
    }
    return 1;
}

int Inject(int fd, unsigned index, const std::array<uint8_t, 6>& router_mac, bool df,
           const char* destination = "198.51.100.2", uint16_t ident = 0,
           const char* source = "192.0.2.2", size_t length = 1028,
           bool reply = false, bool options = false, bool extended_options = false) {
    const size_t header_len = extended_options ? 44 : options ? 40 : 20;
    std::vector<uint8_t> ip(length + header_len - 20, 0x5a);
    std::fill_n(ip.begin(), header_len + 8, 0);
    ip[0] = static_cast<uint8_t>(0x40 | header_len / 4);
    ip[2] = ip.size() >> 8;
    ip[3] = ip.size();
    if (!ident) ident = df ? 0x3702 : 0x3701;
    ip[4] = ident >> 8;
    ip[5] = ident;
    ip[6] = df ? 0x40 : 0;
    ip[8] = 64;
    ip[9] = IPPROTO_UDP;
    inet_pton(AF_INET, source, &ip[12]);
    inet_pton(AF_INET, destination, &ip[16]);
    ip[header_len] = 0xd0;
    ip[header_len + 1] = reply ? 0x38 : 0x37;
    ip[header_len + 2] = 0xd0;
    ip[header_len + 3] = reply ? 0x37 : 0x38;
    const uint16_t udp_len = static_cast<uint16_t>(length - 20);
    ip[header_len + 4] = udp_len >> 8;
    ip[header_len + 5] = udp_len;
    ip[header_len + 6] = ip[header_len + 7] = 0;
    const uint16_t checksum = Checksum(ip.data(), header_len);
    ip[10] = checksum >> 8;
    ip[11] = checksum;
    sockaddr_ll address{};
    address.sll_family = AF_PACKET;
    address.sll_protocol = htons(ETH_P_IP);
    address.sll_ifindex = index;
    address.sll_halen = ETH_ALEN;
    std::memcpy(address.sll_addr, router_mac.data(), router_mac.size());
    return sendto(fd, ip.data(), ip.size(), 0, reinterpret_cast<sockaddr*>(&address),
                  sizeof(address)) == static_cast<ssize_t>(ip.size()) ? 0 : errno;
}

int CaptureSingleUdp(int fd, uint16_t ident, const char* destination) {
    in_addr expected{};
    inet_pton(AF_INET, destination, &expected);
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(2);
    while (std::chrono::steady_clock::now() < deadline) {
        pollfd pfd{fd, POLLIN, 0};
        if (poll(&pfd, 1, 100) <= 0) continue;
        std::array<uint8_t, 2048> bytes{};
        sockaddr_ll source{};
        socklen_t size = sizeof(source);
        const ssize_t count = recvfrom(fd, bytes.data(), bytes.size(), 0,
                                       reinterpret_cast<sockaddr*>(&source), &size);
        if (count < 28 || source.sll_pkttype == PACKET_OUTGOING || bytes[0] >> 4 != 4 ||
            bytes[9] != IPPROTO_UDP || bytes[4] != (ident >> 8) || bytes[5] != (ident & 0xff) ||
            std::memcmp(&bytes[16], &expected, sizeof(expected))) continue;
        return 0;
    }
    return 1;
}

int InjectFragments(int fd, unsigned index, const std::array<uint8_t, 6>& router_mac) {
    std::vector<uint8_t> datagram(1028, 0x5a);
    std::fill_n(datagram.begin(), 28, 0);
    datagram[0] = 0x45;
    datagram[4] = 0x37;
    datagram[5] = 0x05;
    datagram[8] = 64;
    datagram[9] = IPPROTO_UDP;
    inet_pton(AF_INET, "192.0.2.2", &datagram[12]);
    inet_pton(AF_INET, "198.51.100.2", &datagram[16]);
    datagram[20] = 0xd0;
    datagram[21] = 0x39;
    datagram[22] = 0xd0;
    datagram[23] = 0x38;
    datagram[24] = 0x03;
    datagram[25] = 0xf0;
    sockaddr_ll address{};
    address.sll_family = AF_PACKET;
    address.sll_protocol = htons(ETH_P_IP);
    address.sll_ifindex = index;
    address.sll_halen = ETH_ALEN;
    std::memcpy(address.sll_addr, router_mac.data(), router_mac.size());
    for (size_t offset : {size_t(0), size_t(496)}) {
        const size_t chunk = offset == 0 ? 496 : 512;
        std::vector<uint8_t> fragment(datagram.begin(), datagram.begin() + 20);
        fragment.insert(fragment.end(), datagram.begin() + 20 + offset,
                        datagram.begin() + 20 + offset + chunk);
        fragment[2] = fragment.size() >> 8;
        fragment[3] = fragment.size();
        const uint16_t flags = static_cast<uint16_t>(offset / 8) | (offset == 0 ? 0x2000 : 0);
        fragment[6] = flags >> 8;
        fragment[7] = flags;
        const uint16_t checksum = Checksum(fragment.data(), 20);
        fragment[10] = checksum >> 8;
        fragment[11] = checksum;
        if (sendto(fd, fragment.data(), fragment.size(), 0,
                   reinterpret_cast<sockaddr*>(&address), sizeof(address)) !=
            static_cast<ssize_t>(fragment.size())) return errno;
    }
    return 0;
}

int Capture(int fd, bool expect_icmp, uint16_t ident, unsigned expected_mtu,
            const char* expected_quote_destination = nullptr,
            const char* expected_outer_source = nullptr,
            int expected_second_offset = -1) {
    int fragments = 0;
    unsigned total_payload = 0;
    bool saw_second_offset = false;
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(2);
    while (std::chrono::steady_clock::now() < deadline) {
        pollfd pfd{fd, POLLIN, 0};
        if (poll(&pfd, 1, 100) <= 0) continue;
        std::array<uint8_t, 2048> bytes{};
        sockaddr_ll source{};
        socklen_t size = sizeof(source);
        ssize_t count = recvfrom(fd, bytes.data(), bytes.size(), 0,
                                 reinterpret_cast<sockaddr*>(&source), &size);
        if (count < 28 || source.sll_pkttype == PACKET_OUTGOING || bytes[0] >> 4 != 4)
            continue;
        const unsigned length = static_cast<unsigned>(count);
        if (expect_icmp) {
            if (bytes[9] != IPPROTO_ICMP || bytes[20] != 3 || bytes[21] != 4 ||
                length < 56 || bytes[24] != 0 || bytes[25] != 0 ||
                (unsigned(bytes[26]) << 8 | bytes[27]) != expected_mtu ||
                bytes[32] != 0x37 || bytes[33] != (ident & 0xff))
                continue;
            in_addr address{};
            if (expected_quote_destination) {
                inet_pton(AF_INET, expected_quote_destination, &address);
                if (std::memcmp(&address, &bytes[44], sizeof(address))) continue;
            }
            if (expected_outer_source) {
                inet_pton(AF_INET, expected_outer_source, &address);
                if (std::memcmp(&address, &bytes[12], sizeof(address))) continue;
            }
            return 0;
        }
        if (bytes[9] != IPPROTO_UDP || bytes[4] != 0x37 || bytes[5] != (ident & 0xff) ||
            length > expected_mtu || bytes[8] != 63) continue;
        ++fragments;
        total_payload += length - 20;
        saw_second_offset |= (unsigned(bytes[6] & 0x1f) << 8 | bytes[7]) ==
                             static_cast<unsigned>(expected_second_offset);
        if (fragments >= 2 && total_payload == 1008 &&
            (expected_second_offset < 0 || saw_second_offset)) return 0;
    }
    return expect_icmp ? 1 : 2;
}

int Scenario() {
    if (unshare(CLONE_NEWUSER | CLONE_NEWNET) != 0) return -1;
    Fd netlink(socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE));
    if (netlink.Get() < 0) return 100 + errno;
    sockaddr_nl address{};
    address.nl_family = AF_NETLINK;
    if (bind(netlink.Get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)) < 0)
        return 200 + errno;
    timeval timeout{2, 0};
    setsockopt(netlink.Get(), SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout));
    if (int e = CreateVeth(netlink.Get(), "d37a", "d37ap", 1)) return 300 + e;
    if (int e = CreateVeth(netlink.Get(), "d37b", "d37bp", 2)) return 400 + e;
    const unsigned a = if_nametoindex("d37a"), ap = if_nametoindex("d37ap");
    const unsigned b = if_nametoindex("d37b"), bp = if_nametoindex("d37bp");
    if (!a || !ap || !b || !bp) return 500;
    if (SetLink(netlink.Get(), a, 3) || SetLink(netlink.Get(), ap, 4) ||
        SetLink(netlink.Get(), b, 5, 576) || SetLink(netlink.Get(), bp, 6) ||
        Address(netlink.Get(), a, 7, "192.0.2.1") ||
        Address(netlink.Get(), b, 8, "198.51.100.1")) return 600;
    std::array<uint8_t, 6> router_mac{}, peer_a_mac{}, peer_b_mac{};
    if (Mac("d37a", &router_mac) || Mac("d37ap", &peer_a_mac) ||
        Mac("d37bp", &peer_b_mac)) return 700;
    if (Neighbor(netlink.Get(), a, 9, "192.0.2.2", peer_a_mac) ||
        Neighbor(netlink.Get(), b, 10, "198.51.100.2", peer_b_mac)) return 800;
    Fd sysctl(open("/proc/sys/net/ipv4/ip_forward", O_WRONLY));
    if (sysctl.Get() < 0 || write(sysctl.Get(), "1", 1) != 1) return 900 + errno;
    Fd incoming(PacketSocket(ap)), outgoing(PacketSocket(bp));
    if (incoming.Get() < 0 || outgoing.Get() < 0) return 1000 + errno;
    if (int e = Inject(incoming.Get(), ap, router_mac, false)) return 1100 + e;
    if (int e = Capture(outgoing.Get(), false, 0x3701, 576)) return 1200 + e;
    if (int e = Inject(incoming.Get(), ap, router_mac, true)) return 1300 + e;
    if (int e = Capture(incoming.Get(), true, 0x3702, 576)) return 1400 + e;
    // The first DNAT packet is still an unconfirmed conntrack Candidate at
    // the early PMTU error. Its RELATED ICMP must reverse the inner quote and
    // outer source without publishing that rejected original flow.
    if (std::system("iptables -t nat -A PREROUTING -d 203.0.113.1 -j DNAT --to-destination 198.51.100.2 2>/dev/null") != 0) {
        utsname name{};
        // Some host CI environments allow a user-netns veth but prohibit
        // nf_tables in that userns. Keep the basic conformance assertions;
        // DragonOS itself must execute the DNAT subcase.
        if (uname(&name) == 0 && !std::strstr(name.release, "dragonos")) return 0;
        return 1500;
    }
    if (int e = Inject(incoming.Get(), ap, router_mac, true, "203.0.113.1", 0x3703))
        return 1600 + e;
    if (int e = Capture(incoming.Get(), true, 0x3703, 576,
                        "203.0.113.1", "203.0.113.1")) return 1700 + e;
    utsname name{};
    if (uname(&name) == 0 && std::strstr(name.release, "dragonos")) {
        // Minimum IPv4 return MTU leaves only a 40-byte IPv4-options quote.
        // Its RELATED NAT identity must not require eight UDP bytes to exist.
        if (SetLink(netlink.Get(), a, 25, 68)) return 1705;
        if (int e = Inject(incoming.Get(), ap, router_mac, true,
                           "203.0.113.1", 0x3708, "192.0.2.2", 1028, false, true))
            return 1706 + e;
        if (int e = Capture(incoming.Get(), true, 0x3708, 576,
                            "203.0.113.1", "203.0.113.1")) return 1707 + e;
        // A 68-byte return path can quote only 40 bytes of an IHL=44
        // trigger. Linux still sends Frag Needed with a partial IP header.
        if (int e = Inject(incoming.Get(), ap, router_mac, true,
                           "198.51.100.2", 0x3709, "192.0.2.2", 1028,
                           false, true, true)) return 1708 + e;
        if (int e = Capture(incoming.Get(), true, 0x3709, 576)) return 1708 + e;
        // The same partial IHL must survive the RELATED DNAT quote rewrite.
        if (int e = Inject(incoming.Get(), ap, router_mac, true,
                           "203.0.113.1", 0x370a, "192.0.2.2", 1028,
                           false, true, true)) return 1711 + e;
        if (int e = Capture(incoming.Get(), true, 0x370a, 576,
                            "203.0.113.1", "203.0.113.1")) return 1712 + e;
        if (int e = SetLink(netlink.Get(), a, 26, 1500)) return 1709 + e;
        if (int e = InjectFragments(incoming.Get(), ap, router_mac)) return 1750 + e;
        if (int e = Capture(outgoing.Get(), false, 0x3705, 532,
                            nullptr, nullptr, 64)) return 1770 + e;
        // Establish the DNAT flow, then force an early PMTU error on its
        // reverse direction. The quote is still before reverse POSTROUTING.
        if (SetLink(netlink.Get(), b, 21, 1500)) return 1710;
        if (int e = Inject(incoming.Get(), ap, router_mac, false,
                           "203.0.113.1", 0x3706, "192.0.2.2", 128)) return 1720 + e;
        if (int e = CaptureSingleUdp(outgoing.Get(), 0x3706, "198.51.100.2")) return 1730 + e;
        if (SetLink(netlink.Get(), a, 22, 576)) return 1740;
        std::array<uint8_t, 6> router_b_mac{};
        if (Mac("d37b", &router_b_mac)) return 1745;
        if (int e = Inject(outgoing.Get(), bp, router_b_mac, true,
                           "192.0.2.2", 0x3707, "198.51.100.2", 1028, true)) return 1750 + e;
        if (int e = Capture(outgoing.Get(), true, 0x3707, 576,
                            "192.0.2.2", "198.51.100.1")) return 1760 + e;
        if (SetLink(netlink.Get(), a, 23, 1500) ||
            SetLink(netlink.Get(), b, 24, 576)) return 1770;
        // Hold the packet in the unresolved-neighbor queue, then shrink the
        // egress MTU. The final transmit decision must return a late ICMP
        // instead of silently dropping this previously admitted DF packet.
        if (SetLink(netlink.Get(), b, 11, 1500)) return 1800;
        if (int e = Inject(incoming.Get(), ap, router_mac, true, "198.51.100.3", 0x3704))
            return 1900 + e;
        usleep(100000);
        if (SetLink(netlink.Get(), b, 12, 576)) return 2000;
        if (Neighbor(netlink.Get(), b, 13, "198.51.100.3", peer_b_mac)) return 2100;
        if (int e = Capture(incoming.Get(), true, 0x3704, 576)) return 2200 + e;
    }
    return 0;
}

int Scenario6() {
    if (unshare(CLONE_NEWUSER | CLONE_NEWNET) != 0) return -1;
    Fd netlink(socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE));
    if (netlink.Get() < 0) return 100 + errno;
    sockaddr_nl address{};
    address.nl_family = AF_NETLINK;
    if (bind(netlink.Get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)) < 0)
        return 200 + errno;
    timeval timeout{2, 0};
    setsockopt(netlink.Get(), SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout));
    if (int e = CreateVeth(netlink.Get(), "d37a", "d37ap", 1)) return 300 + e;
    if (int e = CreateVeth(netlink.Get(), "d37b", "d37bp", 2)) return 400 + e;
    const unsigned a = if_nametoindex("d37a"), ap = if_nametoindex("d37ap");
    const unsigned b = if_nametoindex("d37b"), bp = if_nametoindex("d37bp");
    if (!a || !ap || !b || !bp) return 500;
    if (SetLink(netlink.Get(), a, 3) || SetLink(netlink.Get(), ap, 4) ||
        SetLink(netlink.Get(), b, 5, 1280) || SetLink(netlink.Get(), bp, 6) ||
        Address6(netlink.Get(), a, 7, "fd37:1::1") ||
        Address6(netlink.Get(), b, 8, "fd37:2::1")) return 600;
    std::array<uint8_t, 6> router_mac{}, peer_a_mac{}, peer_b_mac{};
    if (Mac("d37a", &router_mac) || Mac("d37ap", &peer_a_mac) ||
        Mac("d37bp", &peer_b_mac)) return 700;
    if (Neighbor6(netlink.Get(), a, 9, "fd37:1::2", peer_a_mac) ||
        Neighbor6(netlink.Get(), b, 10, "fd37:2::2", peer_b_mac)) return 800;
    Fd sysctl(open("/proc/sys/net/ipv6/conf/all/forwarding", O_WRONLY));
    if (sysctl.Get() < 0 || write(sysctl.Get(), "1", 1) != 1) return 900 + errno;
    Fd incoming(PacketSocket(ap, ETH_P_IPV6));
    if (incoming.Get() < 0) return 1000 + errno;
    if (int e = Inject6(incoming.Get(), ap, router_mac)) return 1100 + e;
    if (int e = Capture6(incoming.Get())) return 1200 + e;
    if (std::system("ip6tables -t nat -A PREROUTING -d fdff::1 -j DNAT --to-destination fd37:2::2 2>/dev/null") != 0) {
        utsname name{};
        if (uname(&name) == 0 && !std::strstr(name.release, "dragonos")) return 0;
        return 1300;
    }
    // A reassembled IPv6 packet may be fragmented again only when the largest
    // original fragment fits the egress path. Keep the latter above IPv6's
    // 1280-byte minimum so its original-fragment cap is observable.
    if (SetLink(netlink.Get(), b, 11, 1500)) return 1350;
    Fd outgoing(PacketSocket(bp, ETH_P_IPV6));
    if (outgoing.Get() < 0) return 1400 + errno;
    if (int e = Inject6Fragments(incoming.Get(), ap, router_mac)) return 1500 + e;
    if (int e = Capture6Fragments(outgoing.Get())) return 1600 + e;
    utsname name{};
    if (uname(&name) == 0 && std::strstr(name.release, "dragonos")) {
        // An explicit route survives address removal on an MTU shrink.
        // Never answer an oversized datagram with PTB(1280) for a link
        // that can only transmit 576-byte frames.
        if (int e = Route6(netlink.Get(), b, 12, "fd37:2::2")) return 1700 + e;
        if (SetLink(netlink.Get(), b, 13, 576)) return 1800;
        if (int e = Inject6(incoming.Get(), ap, router_mac, 0xd03a)) return 1900 + e;
        pollfd pfd{incoming.Get(), POLLIN, 0};
        const auto deadline = std::chrono::steady_clock::now() +
                              std::chrono::milliseconds(300);
        while (std::chrono::steady_clock::now() < deadline) {
            if (poll(&pfd, 1, 30) <= 0) continue;
            std::array<uint8_t, 2048> bytes{};
            sockaddr_ll source{};
            socklen_t size = sizeof(source);
            ssize_t count = recvfrom(incoming.Get(), bytes.data(), bytes.size(), 0,
                                     reinterpret_cast<sockaddr*>(&source), &size);
            if (count >= 90 && source.sll_pkttype != PACKET_OUTGOING &&
                bytes[0] >> 4 == 6 && bytes[6] == 58 && bytes[40] == 2 &&
                bytes[88] == 0xd0 && bytes[89] == 0x3a)
                return 2000;
        }
    }
    return 0;
}

}  // namespace

TEST(ForwardMtu, Ipv4FragmentsAndReturnsFragNeeded) {
    int pipe_fds[2];
    ASSERT_EQ(pipe(pipe_fds), 0);
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(pipe_fds[0]);
        const int result = Scenario();
        const ssize_t written = write(pipe_fds[1], &result, sizeof(result));
        (void)written;
        _exit(result == 0 || result == -1 ? 0 : 1);
    }
    close(pipe_fds[1]);
    int status = 0;
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(10);
    while (waitpid(child, &status, WNOHANG) == 0) {
        if (std::chrono::steady_clock::now() <= deadline) {
            usleep(10000);
            continue;
        }
        kill(child, SIGKILL);
        waitpid(child, &status, 0);
        close(pipe_fds[0]);
        FAIL() << "forward MTU child timed out";
    }
    int result = 0;
    ASSERT_EQ(read(pipe_fds[0], &result, sizeof(result)), static_cast<ssize_t>(sizeof(result)));
    close(pipe_fds[0]);
    if (result == -1) {
        utsname name{};
        if (uname(&name) == 0 && std::strstr(name.release, "dragonos"))
            FAIL() << "DragonOS network namespace unavailable";
        GTEST_SKIP() << "network namespace unavailable on host";
    }
    EXPECT_EQ(result, 0) << "stage/error=" << result;
    EXPECT_TRUE(WIFEXITED(status));
    EXPECT_EQ(WEXITSTATUS(status), 0);
}

TEST(ForwardMtu, Ipv6ReturnsPacketTooBig) {
    int pipe_fds[2];
    ASSERT_EQ(pipe(pipe_fds), 0);
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(pipe_fds[0]);
        const int result = Scenario6();
        const ssize_t written = write(pipe_fds[1], &result, sizeof(result));
        (void)written;
        _exit(result == 0 || result == -1 ? 0 : 1);
    }
    close(pipe_fds[1]);
    int status = 0;
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(10);
    while (waitpid(child, &status, WNOHANG) == 0) {
        if (std::chrono::steady_clock::now() <= deadline) {
            usleep(10000);
            continue;
        }
        kill(child, SIGKILL);
        waitpid(child, &status, 0);
        close(pipe_fds[0]);
        FAIL() << "forward MTU IPv6 child timed out";
    }
    int result = 0;
    ASSERT_EQ(read(pipe_fds[0], &result, sizeof(result)), static_cast<ssize_t>(sizeof(result)));
    close(pipe_fds[0]);
    if (result == -1) {
        utsname name{};
        if (uname(&name) == 0 && std::strstr(name.release, "dragonos"))
            FAIL() << "DragonOS network namespace unavailable";
        GTEST_SKIP() << "network namespace unavailable on host";
    }
    EXPECT_EQ(result, 0) << "stage/error=" << result;
    EXPECT_TRUE(WIFEXITED(status));
    EXPECT_EQ(WEXITSTATUS(status), 0);
}

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
