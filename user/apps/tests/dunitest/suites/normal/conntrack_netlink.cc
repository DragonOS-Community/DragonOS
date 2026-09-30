#include <gtest/gtest.h>

#include <linux/netfilter/nf_conntrack_common.h>
#include <linux/netfilter/nfnetlink_conntrack.h>
#include <net/if.h>
#include <sched.h>
#include <sys/ioctl.h>
#include <sys/wait.h>

#include <algorithm>
#include <atomic>
#include <set>
#include <thread>

#include "forward_mtu_nft_support.h"

namespace {
using Bytes = std::vector<uint8_t>;
using namespace forward_mtu_nft;

struct Reply {
    int error = 0;
    bool done = false;
    std::vector<Bytes> records;
    std::vector<uint16_t> record_flags;
};

// One transaction per socket avoids ACKs from one request contaminating the next.
Reply Exchange(const Bytes& request, unsigned acknowledgements = 1) {
    Reply result;
    int fd = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_NETFILTER);
    if (fd < 0) { result.error = errno; return result; }
    timeval timeout{3, 0};
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout));
    sockaddr_nl destination{};
    destination.nl_family = AF_NETLINK;
    if (sendto(fd, request.data(), request.size(), 0,
               reinterpret_cast<sockaddr*>(&destination), sizeof(destination)) !=
        static_cast<ssize_t>(request.size())) {
        result.error = errno;
        close(fd);
        return result;
    }
    const auto* sent = reinterpret_cast<const nlmsghdr*>(request.data());
    const bool dump = (sent->nlmsg_flags & NLM_F_DUMP) == NLM_F_DUMP;
    unsigned received = 0;
    while (!result.done && !result.error) {
        std::array<uint8_t, 65536> buffer{};
        ssize_t size = recv(fd, buffer.data(), buffer.size(), 0);
        if (size < 0) { result.error = errno; break; }
        if (size == 0) { result.error = EPROTO; break; }
        int left = size;
        for (auto* h = reinterpret_cast<nlmsghdr*>(buffer.data());
             NLMSG_OK(h, left); h = NLMSG_NEXT(h, left)) {
            if (h->nlmsg_type == NLMSG_ERROR) {
                if (h->nlmsg_len < NLMSG_LENGTH(sizeof(nlmsgerr))) {
                    result.error = EPROTO;
                    break;
                }
                result.error = -reinterpret_cast<nlmsgerr*>(NLMSG_DATA(h))->error;
                if (++received == acknowledgements && !dump) result.done = true;
            } else if (h->nlmsg_type == NLMSG_DONE) {
                if (h->nlmsg_len >= NLMSG_LENGTH(sizeof(int)))
                    std::memcpy(&result.error, NLMSG_DATA(h), sizeof(int));
                result.error = std::abs(result.error);
                result.done = true;
            } else {
                const auto* begin = reinterpret_cast<const uint8_t*>(NLMSG_DATA(h));
                result.records.emplace_back(begin, begin + NLMSG_PAYLOAD(h, 0));
                result.record_flags.push_back(h->nlmsg_flags);
            }
        }
    }
    close(fd);
    return result;
}

Bytes CtRequest(uint8_t operation, uint8_t family, bool dump = false) {
    auto bytes = Message((NFNL_SUBSYS_CTNETLINK << 8) | operation,
                         4900, family, dump ? NLM_F_DUMP : 0);
    if (dump) reinterpret_cast<nlmsghdr*>(bytes.data())->nlmsg_flags &= ~NLM_F_ACK;
    return bytes;
}

Bytes Attribute(const Bytes& record, uint16_t kind) {
    size_t offset = sizeof(nfgenmsg);
    while (offset + sizeof(nlattr) <= record.size()) {
        const auto* attr = reinterpret_cast<const nlattr*>(record.data() + offset);
        if (attr->nla_len < sizeof(nlattr) || offset + attr->nla_len > record.size())
            return {};
        if ((attr->nla_type & NLA_TYPE_MASK) == kind)
            return Bytes(record.begin() + offset, record.begin() + offset + NLA_ALIGN(attr->nla_len));
        offset += NLA_ALIGN(attr->nla_len);
    }
    return {};
}

uint32_t Value(const Bytes& record, uint16_t kind) {
    auto attr = Attribute(record, kind);
    if (attr.size() < sizeof(nlattr) + sizeof(uint32_t)) return 0;
    uint32_t value;
    std::memcpy(&value, attr.data() + sizeof(nlattr), sizeof(value));
    return ntohl(value);
}

void AppendAttrs(Bytes* request, const Bytes& attrs) {
    request->insert(request->end(), attrs.begin(), attrs.end());
    reinterpret_cast<nlmsghdr*>(request->data())->nlmsg_len = request->size();
}

int ActivateConntrack() {
    auto batch = Message(NFNL_MSG_BATCH_BEGIN, 4900, AF_UNSPEC);
    auto table = Message((NFNL_SUBSYS_NFTABLES << 8) | NFT_MSG_NEWTABLE,
                         4901, NFPROTO_INET, NLM_F_CREATE | NLM_F_EXCL);
    TextAttr(&table, NFTA_TABLE_NAME, "cttest");
    Append(&batch, table);
    auto chain = Message((NFNL_SUBSYS_NFTABLES << 8) | NFT_MSG_NEWCHAIN,
                         4902, NFPROTO_INET, NLM_F_CREATE | NLM_F_EXCL);
    TextAttr(&chain, NFTA_CHAIN_TABLE, "cttest");
    TextAttr(&chain, NFTA_CHAIN_NAME, "output");
    TextAttr(&chain, NFTA_CHAIN_TYPE, "filter");
    Bytes hook;
    Be32(&hook, NFTA_HOOK_HOOKNUM, NF_INET_LOCAL_OUT);
    Be32(&hook, NFTA_HOOK_PRIORITY, static_cast<uint32_t>(-150));
    TopAttr(&chain, NFTA_CHAIN_HOOK | NLA_F_NESTED, hook.data(), hook.size());
    Append(&batch, chain);
    auto rule = Message((NFNL_SUBSYS_NFTABLES << 8) | NFT_MSG_NEWRULE,
                        4903, NFPROTO_INET, NLM_F_CREATE | NLM_F_APPEND);
    TextAttr(&rule, NFTA_RULE_TABLE, "cttest");
    TextAttr(&rule, NFTA_RULE_CHAIN, "output");
    Bytes expressions, ct;
    Be32(&ct, NFTA_CT_KEY, NFT_CT_STATE);
    Be32(&ct, NFTA_CT_DREG, NFT_REG_1);
    Expression(&expressions, "ct", ct);
    TopAttr(&rule, NFTA_RULE_EXPRESSIONS | NLA_F_NESTED,
            expressions.data(), expressions.size());
    Append(&batch, rule);
    auto end = Message(NFNL_MSG_BATCH_END, 4904, AF_UNSPEC);
    reinterpret_cast<nlmsghdr*>(end.data())->nlmsg_flags &= ~NLM_F_ACK;
    Append(&batch, end);
    return Exchange(batch, 3).error;
}

class ConntrackNetlink : public testing::Test {
protected:
    std::vector<int> sockets;
    void SetUp() override {
        ASSERT_EQ(unshare(CLONE_NEWNET), 0) << strerror(errno);
        int fd = socket(AF_INET, SOCK_DGRAM, 0);
        ASSERT_GE(fd, 0);
        ifreq interface{};
        std::strcpy(interface.ifr_name, "lo");
        ASSERT_EQ(ioctl(fd, SIOCGIFFLAGS, &interface), 0);
        interface.ifr_flags |= IFF_UP;
        ASSERT_EQ(ioctl(fd, SIOCSIFFLAGS, &interface), 0);
        close(fd);
        ASSERT_EQ(ActivateConntrack(), 0);
    }
    void TearDown() override { for (int fd : sockets) close(fd); }
    int Socket(int family, int type) {
        int fd = socket(family, type | SOCK_CLOEXEC, 0);
        if (fd >= 0) {
            timeval timeout{3, 0};
            setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout));
            setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout));
            sockets.push_back(fd);
        }
        return fd;
    }
    void Flow(int family, int type = SOCK_DGRAM) {
        int server = Socket(family, type), client = Socket(family, type);
        ASSERT_GE(server, 0);
        ASSERT_GE(client, 0);
        sockaddr_storage address{};
        socklen_t length;
        if (family == AF_INET) {
            auto* a = reinterpret_cast<sockaddr_in*>(&address);
            a->sin_family = AF_INET;
            a->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
            length = sizeof(*a);
        } else {
            auto* a = reinterpret_cast<sockaddr_in6*>(&address);
            a->sin6_family = AF_INET6;
            a->sin6_addr = in6addr_loopback;
            length = sizeof(*a);
        }
        ASSERT_EQ(bind(server, reinterpret_cast<sockaddr*>(&address), length), 0);
        ASSERT_EQ(getsockname(server, reinterpret_cast<sockaddr*>(&address), &length), 0);
        if (type == SOCK_STREAM) { ASSERT_EQ(listen(server, 1), 0); }
        ASSERT_EQ(connect(client, reinterpret_cast<sockaddr*>(&address), length), 0);
        if (type == SOCK_STREAM) {
            int accepted = accept(server, nullptr, nullptr);
            ASSERT_GE(accepted, 0);
            sockets.push_back(accepted);
        }
        ASSERT_EQ(send(client, "x", 1, 0), 1);
    }
    Reply Dump(int family = AF_UNSPEC) {
        return Exchange(CtRequest(IPCTNL_MSG_CT_GET, family, true));
    }
};

TEST_F(ConntrackNetlink, EmptyTableAndMalformedRequests) {
    auto result = Dump();
    ASSERT_EQ(result.error, 0);
    EXPECT_TRUE(result.done);
    EXPECT_TRUE(result.records.empty());
    EXPECT_EQ(Exchange(CtRequest(IPCTNL_MSG_CT_GET, AF_INET)).error, EINVAL);
    auto bad = CtRequest(IPCTNL_MSG_CT_DELETE, AF_INET);
    uint16_t short_value = 0;
    TopAttr(&bad, CTA_ID, &short_value, sizeof(short_value));
    EXPECT_EQ(Exchange(bad).error, ERANGE);
}

TEST_F(ConntrackNetlink, IPv4IPv6UdpTcpExactAndRawDelete) {
    for (int family : {AF_INET, AF_INET6}) {
        Flow(family);
        Flow(family, SOCK_STREAM);
        ASSERT_FALSE(HasFatalFailure());
        auto dump = Dump(family);
        ASSERT_EQ(dump.error, 0);
        ASSERT_GE(dump.records.size(), 2u);
        for (const auto& record : dump.records) {
            EXPECT_NE(Value(record, CTA_ID), 0u);
            EXPECT_NE(Value(record, CTA_STATUS) & IPS_CONFIRMED, 0u);
            for (int direction : {CTA_TUPLE_ORIG, CTA_TUPLE_REPLY}) {
                auto get = CtRequest(IPCTNL_MSG_CT_GET, family);
                AppendAttrs(&get, Attribute(record, direction));
                auto exact = Exchange(get);
                ASSERT_EQ(exact.error, 0);
                ASSERT_EQ(exact.records.size(), 1u);
                EXPECT_EQ(Value(exact.records[0], CTA_ID), Value(record, CTA_ID));
            }
            auto remove = CtRequest(IPCTNL_MSG_CT_DELETE, family);
            AppendAttrs(&remove, Bytes(record.begin() + sizeof(nfgenmsg), record.end()));
            EXPECT_EQ(Exchange(remove).error, 0);
            EXPECT_EQ(Exchange(remove).error, ENOENT);
        }
        EXPECT_TRUE(Dump(family).records.empty());
    }
}

TEST_F(ConntrackNetlink, StaleIdCannotDeleteTupleReplacement) {
    Flow(AF_INET);
    auto dump = Dump(AF_INET);
    ASSERT_EQ(dump.error, 0);
    ASSERT_EQ(dump.records.size(), 1u);
    const auto& record = dump.records[0];
    auto remove = CtRequest(IPCTNL_MSG_CT_DELETE, AF_INET);
    AppendAttrs(&remove, Attribute(record, CTA_TUPLE_ORIG));
    uint32_t wrong = htonl(Value(record, CTA_ID) ^ 0x80000000u);
    TopAttr(&remove, CTA_ID, &wrong, sizeof(wrong));
    EXPECT_EQ(Exchange(remove).error, ENOENT);
    EXPECT_EQ(Dump(AF_INET).records.size(), 1u);
    auto get = remove;
    reinterpret_cast<nlmsghdr*>(get.data())->nlmsg_type =
        (NFNL_SUBSYS_CTNETLINK << 8) | IPCTNL_MSG_CT_GET;
    EXPECT_EQ(Exchange(get).error, 0);  // GET ignores an optional ID.
    auto raw = CtRequest(IPCTNL_MSG_CT_DELETE, AF_INET);
    AppendAttrs(&raw, Bytes(record.begin() + sizeof(nfgenmsg), record.end()));
    ASSERT_EQ(Exchange(raw).error, 0);
    ASSERT_EQ(send(sockets[1], "y", 1, 0), 1);
    auto replacement = Dump(AF_INET);
    ASSERT_EQ(replacement.error, 0);
    ASSERT_EQ(replacement.records.size(), 1u);
    // Linux hashes the object address and tuple for its ID: allocator reuse can
    // collide. Verify ID protection deterministically even in that case.
    if (Value(replacement.records[0], CTA_ID) == Value(record, CTA_ID)) {
        raw = remove;
    }
    EXPECT_EQ(Exchange(raw).error, ENOENT);
    EXPECT_EQ(Dump(AF_INET).records.size(), 1u);
}

TEST_F(ConntrackNetlink, FlushVersionAndStatusMasks) {
    Flow(AF_INET);
    Flow(AF_INET6);
    ASSERT_FALSE(HasFatalFailure());
    auto filtered = CtRequest(IPCTNL_MSG_CT_GET, AF_UNSPEC, true);
    uint32_t status = htonl(IPS_CONFIRMED);
    TopAttr(&filtered, CTA_STATUS, &status, sizeof(status));
    EXPECT_EQ(Exchange(filtered).records.size(), 2u);
    auto invalid = filtered;
    uint32_t zero = 0;
    TopAttr(&invalid, CTA_STATUS_MASK, &zero, sizeof(zero));
    EXPECT_EQ(Exchange(invalid).error, EINVAL);
    uint32_t mask = htonl(IPS_CONFIRMED);
    TopAttr(&filtered, CTA_STATUS_MASK, &mask, sizeof(mask));
    auto selected = Exchange(filtered);
    ASSERT_EQ(selected.error, 0);
    ASSERT_EQ(selected.records.size(), 2u);
    for (auto flags : selected.record_flags)
        EXPECT_NE(flags & NLM_F_DUMP_FILTERED, 0);
    auto unfiltered = Dump();
    for (auto flags : unfiltered.record_flags)
        EXPECT_EQ(flags & NLM_F_DUMP_FILTERED, 0);
    auto flush = CtRequest(IPCTNL_MSG_CT_DELETE, AF_INET);
    reinterpret_cast<nfgenmsg*>(NLMSG_DATA(reinterpret_cast<nlmsghdr*>(flush.data())))->version = 1;
    ASSERT_EQ(Exchange(flush).error, 0);
    EXPECT_TRUE(Dump(AF_INET).records.empty());
    EXPECT_EQ(Dump(AF_INET6).records.size(), 1u);
    reinterpret_cast<nfgenmsg*>(NLMSG_DATA(reinterpret_cast<nlmsghdr*>(flush.data())))->version = 0;
    ASSERT_EQ(Exchange(flush).error, 0);
    EXPECT_TRUE(Dump().records.empty());
}

TEST_F(ConntrackNetlink, MultipartDumpAndNamespaceIsolation) {
    for (int i = 0; i < 70; ++i) Flow(AF_INET);
    ASSERT_FALSE(HasFatalFailure());
    auto dump = Dump();
    ASSERT_EQ(dump.error, 0);
    ASSERT_TRUE(dump.done);
    ASSERT_EQ(dump.records.size(), 70u);
    std::set<uint32_t> ids;
    for (const auto& record : dump.records) ids.insert(Value(record, CTA_ID));
    EXPECT_EQ(ids.size(), 70u);
    EXPECT_EQ(unshare(CLONE_NEWNET), 0);
    auto isolated = Dump();
    EXPECT_EQ(isolated.error, 0);
    EXPECT_TRUE(isolated.records.empty());
}

TEST_F(ConntrackNetlink, TupleFiltersAndDeprecatedNestedEncoding) {
    Flow(AF_INET);
    Flow(AF_INET);
    ASSERT_FALSE(HasFatalFailure());
    auto dump = Dump(AF_INET);
    ASSERT_EQ(dump.error, 0);
    ASSERT_EQ(dump.records.size(), 2u);
    for (uint16_t direction : {CTA_TUPLE_ORIG, CTA_TUPLE_REPLY}) {
        auto filtered = CtRequest(IPCTNL_MSG_CT_GET, AF_INET, true);
        AppendAttrs(&filtered, Attribute(dump.records[0], direction));
        Bytes filter;
        // Linux nf_internals.h flags: protocol number and source/destination
        // port. Selecting both ports distinguishes the two loopback flows.
        uint32_t flags = (1u << 3) | (1u << 4) | (1u << 5);
        Attr(&filter, direction == CTA_TUPLE_ORIG ? CTA_FILTER_ORIG_FLAGS :
             CTA_FILTER_REPLY_FLAGS, &flags, sizeof(flags));
        TopAttr(&filtered, CTA_FILTER | NLA_F_NESTED, filter.data(), filter.size());
        auto result = Exchange(filtered);
        ASSERT_EQ(result.error, 0);
        ASSERT_EQ(result.records.size(), 1u);
        ASSERT_EQ(result.record_flags.size(), 1u);
        EXPECT_NE(result.record_flags[0] & NLM_F_DUMP_FILTERED, 0);
        EXPECT_EQ(Value(result.records[0], CTA_ID), Value(dump.records[0], CTA_ID));
    }
    auto tuple = Attribute(dump.records[0], CTA_TUPLE_ORIG);
    reinterpret_cast<nlattr*>(tuple.data())->nla_type &= NLA_TYPE_MASK;
    for (size_t offset = sizeof(nlattr); offset + sizeof(nlattr) <= tuple.size();) {
        auto* nested = reinterpret_cast<nlattr*>(tuple.data() + offset);
        ASSERT_GE(nested->nla_len, sizeof(nlattr));
        nested->nla_type &= NLA_TYPE_MASK;
        offset += NLA_ALIGN(nested->nla_len);
    }
    auto get = CtRequest(IPCTNL_MSG_CT_GET, AF_INET);
    AppendAttrs(&get, tuple);
    auto exact = Exchange(get);
    ASSERT_EQ(exact.error, 0);
    ASSERT_EQ(exact.records.size(), 1u);
    EXPECT_EQ(Value(exact.records[0], CTA_ID), Value(dump.records[0], CTA_ID));
    // UDP's policy ignores ICMP metadata, even an empty ICMP-only field.
    auto protocol = Attribute(tuple, CTA_TUPLE_PROTO);
    uint8_t unused = 0;
    Attr(&protocol, CTA_PROTO_ICMP_TYPE, &unused, 0);
    reinterpret_cast<nlattr*>(protocol.data())->nla_len = protocol.size();
    auto compatible_tuple = Attribute(dump.records[0], CTA_TUPLE_ORIG);
    compatible_tuple.resize(sizeof(nlattr));
    auto ip = Attribute(tuple, CTA_TUPLE_IP);
    compatible_tuple.insert(compatible_tuple.end(), ip.begin(), ip.end());
    compatible_tuple.insert(compatible_tuple.end(), protocol.begin(), protocol.end());
    reinterpret_cast<nlattr*>(compatible_tuple.data())->nla_len = compatible_tuple.size();
    auto compatible = CtRequest(IPCTNL_MSG_CT_GET, AF_INET);
    AppendAttrs(&compatible, compatible_tuple);
    EXPECT_EQ(Exchange(compatible).error, 0);
    // Legal ignored dump metadata must not change an exact GET or DELETE.
    uint32_t marker = htonl(42);
    TopAttr(&get, CTA_MARK, &marker, sizeof(marker));
    EXPECT_EQ(Exchange(get).error, 0);
    auto malformed = CtRequest(IPCTNL_MSG_CT_DELETE, AF_INET);
    AppendAttrs(&malformed, tuple);
    uint8_t truncated = 1;
    TopAttr(&malformed, CTA_PROTOINFO | NLA_F_NESTED, &truncated, sizeof(truncated));
    EXPECT_EQ(Exchange(malformed).error, ERANGE);
    EXPECT_EQ(Dump(AF_INET).records.size(), 2u);
    // Each duplicate is validated before last-value-wins selection. A later
    // valid value must not hide malformed input and permit a destructive op.
    TopAttr(&malformed, CTA_PROTOINFO | NLA_F_NESTED, &truncated, 0);
    EXPECT_EQ(Exchange(malformed).error, ERANGE);
    auto duplicate_id = CtRequest(IPCTNL_MSG_CT_DELETE, AF_INET);
    AppendAttrs(&duplicate_id, tuple);
    uint16_t short_id = 0;
    TopAttr(&duplicate_id, CTA_ID, &short_id, sizeof(short_id));
    uint32_t valid_id = htonl(Value(dump.records[0], CTA_ID));
    TopAttr(&duplicate_id, CTA_ID, &valid_id, sizeof(valid_id));
    EXPECT_EQ(Exchange(duplicate_id).error, ERANGE);
    EXPECT_EQ(Dump(AF_INET).records.size(), 2u);
    auto flush = CtRequest(IPCTNL_MSG_CT_DELETE, AF_INET);
    Bytes filter;
    uint32_t flags = 1;
    Attr(&filter, CTA_FILTER_ORIG_FLAGS, &flags, sizeof(flags));
    TopAttr(&flush, CTA_FILTER | NLA_F_NESTED, filter.data(), filter.size());
    EXPECT_EQ(Exchange(flush).error, EOPNOTSUPP);
    EXPECT_EQ(Dump(AF_INET).records.size(), 2u);
}

TEST_F(ConntrackNetlink, UnprivilegedQueryAndDeleteAreDenied) {
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (setgid(65534) != 0 || setuid(65534) != 0) _exit(2);
        int get = Exchange(CtRequest(IPCTNL_MSG_CT_GET, AF_INET, true)).error;
        int del = Exchange(CtRequest(IPCTNL_MSG_CT_DELETE, AF_INET)).error;
        _exit(get == EPERM && del == EPERM ? 0 : 1);
    }
    int status;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(WEXITSTATUS(status), 0);
}

TEST_F(ConntrackNetlink, ConcurrentDeleteHasOneWinner) {
    Flow(AF_INET);
    ASSERT_FALSE(HasFatalFailure());
    auto dump = Dump(AF_INET);
    ASSERT_EQ(dump.error, 0);
    ASSERT_EQ(dump.records.size(), 1u);
    auto remove = CtRequest(IPCTNL_MSG_CT_DELETE, AF_INET);
    AppendAttrs(&remove, Bytes(dump.records[0].begin() + sizeof(nfgenmsg),
                              dump.records[0].end()));
    std::atomic<unsigned> winners{0}, missing{0}, unexpected{0};
    std::vector<std::thread> workers;
    for (unsigned i = 0; i < 8; ++i) {
        workers.emplace_back([&] {
            int error = Exchange(remove).error;
            if (error == 0) ++winners;
            else if (error == ENOENT) ++missing;
            else ++unexpected;
        });
    }
    for (auto& worker : workers) worker.join();
    EXPECT_EQ(winners.load(), 1u);
    EXPECT_EQ(missing.load(), 7u);
    EXPECT_EQ(unexpected.load(), 0u);
    EXPECT_TRUE(Dump().records.empty());
}

TEST_F(ConntrackNetlink, UdpAssuredRequiresTrafficAfterStreamThreshold) {
    Flow(AF_INET);
    ASSERT_FALSE(HasFatalFailure());
    auto initial = Dump(AF_INET);
    ASSERT_EQ(initial.error, 0);
    ASSERT_EQ(initial.records.size(), 1u);
    EXPECT_EQ(Value(initial.records[0], CTA_STATUS) & IPS_ASSURED, 0u);
    sockaddr_storage peer{};
    socklen_t length = sizeof(peer);
    char byte;
    ASSERT_EQ(recvfrom(sockets[0], &byte, 1, 0,
                      reinterpret_cast<sockaddr*>(&peer), &length), 1);
    ASSERT_EQ(sendto(sockets[0], "r", 1, 0,
                    reinterpret_cast<sockaddr*>(&peer), length), 1);
    ASSERT_EQ(recv(sockets[1], &byte, 1, 0), 1);
    auto replied = Dump(AF_INET);
    ASSERT_EQ(replied.error, 0);
    ASSERT_EQ(replied.records.size(), 1u);
    EXPECT_NE(Value(replied.records[0], CTA_STATUS) & IPS_SEEN_REPLY, 0u);
    EXPECT_EQ(Value(replied.records[0], CTA_STATUS) & IPS_ASSURED, 0u);
    sleep(3);
    // A read must not manufacture ASSURED merely because time elapsed.
    auto quiet = Dump(AF_INET);
    ASSERT_EQ(quiet.error, 0);
    ASSERT_EQ(quiet.records.size(), 1u);
    EXPECT_EQ(Value(quiet.records[0], CTA_STATUS) & IPS_ASSURED, 0u);
    ASSERT_EQ(send(sockets[1], "s", 1, 0), 1);
    auto stream = Dump(AF_INET);
    ASSERT_EQ(stream.error, 0);
    ASSERT_EQ(stream.records.size(), 1u);
    EXPECT_NE(Value(stream.records[0], CTA_STATUS) & IPS_ASSURED, 0u);
}
}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
