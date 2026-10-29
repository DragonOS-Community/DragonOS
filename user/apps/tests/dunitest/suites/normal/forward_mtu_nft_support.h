#pragma once

// Minimal nf_tables setup for the forwarding-MTU test. Dunitest's default
// rootfs has the kernel ABI but intentionally does not ship iptables binaries.
#include <arpa/inet.h>
#include <linux/netfilter.h>
#include <linux/netfilter/nf_tables.h>
#include <linux/netfilter/nfnetlink.h>
#include <linux/netlink.h>
#include <sys/socket.h>
#include <unistd.h>

#include <array>
#include <cerrno>
#include <cstdint>
#include <cstring>
#include <vector>

namespace forward_mtu_nft {

inline void Attr(std::vector<uint8_t>* bytes, uint16_t type,
                 const void* value, size_t length) {
    const size_t offset = bytes->size();
    bytes->resize(offset + NLA_ALIGN(sizeof(nlattr) + length), 0);
    auto* attr = reinterpret_cast<nlattr*>(bytes->data() + offset);
    attr->nla_type = type;
    attr->nla_len = sizeof(nlattr) + length;
    if (length) std::memcpy(bytes->data() + offset + sizeof(nlattr), value, length);
}

inline void TopAttr(std::vector<uint8_t>* message, uint16_t type,
                    const void* value, size_t length) {
    const size_t offset = message->size();
    Attr(message, type, value, length);
    reinterpret_cast<nlmsghdr*>(message->data())->nlmsg_len =
        offset + sizeof(nlattr) + length;
}

inline void TextAttr(std::vector<uint8_t>* message, uint16_t type,
                     const char* value) {
    TopAttr(message, type, value, std::strlen(value) + 1);
}

inline void Be32(std::vector<uint8_t>* bytes, uint16_t type, uint32_t value) {
    const uint32_t encoded = htonl(value);
    Attr(bytes, type, &encoded, sizeof(encoded));
}

inline std::vector<uint8_t> Message(uint16_t type, uint32_t seq,
                                    uint8_t family, uint16_t flags = 0) {
    std::vector<uint8_t> bytes(NLMSG_SPACE(sizeof(nfgenmsg)), 0);
    auto* header = reinterpret_cast<nlmsghdr*>(bytes.data());
    header->nlmsg_len = NLMSG_LENGTH(sizeof(nfgenmsg));
    header->nlmsg_type = type;
    header->nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK | flags;
    header->nlmsg_seq = seq;
    auto* gen = reinterpret_cast<nfgenmsg*>(NLMSG_DATA(header));
    gen->nfgen_family = family;
    gen->version = NFNETLINK_V0;
    if (type == NFNL_MSG_BATCH_BEGIN) gen->res_id = htons(NFNL_SUBSYS_NFTABLES);
    return bytes;
}

inline void Expression(std::vector<uint8_t>* list, const char* name,
                       const std::vector<uint8_t>& data) {
    std::vector<uint8_t> expr;
    Attr(&expr, NFTA_EXPR_NAME, name, std::strlen(name) + 1);
    Attr(&expr, NFTA_EXPR_DATA | NLA_F_NESTED, data.data(), data.size());
    Attr(list, NFTA_LIST_ELEM | NLA_F_NESTED, expr.data(), expr.size());
}

inline void Append(std::vector<uint8_t>* batch, const std::vector<uint8_t>& message) {
    batch->insert(batch->end(), message.begin(), message.end());
}

inline int InstallDnat(uint8_t family, const char* original, const char* target) {
    constexpr uint16_t kNewTable = (NFNL_SUBSYS_NFTABLES << 8) | NFT_MSG_NEWTABLE;
    constexpr uint16_t kNewChain = (NFNL_SUBSYS_NFTABLES << 8) | NFT_MSG_NEWCHAIN;
    constexpr uint16_t kNewRule = (NFNL_SUBSYS_NFTABLES << 8) | NFT_MSG_NEWRULE;
    constexpr uint32_t kSeq = 3700;
    constexpr char kTable[] = "nat";
    constexpr char kChain[] = "PREROUTING";
    const size_t address_size = family == NFPROTO_IPV4 ? 4 : 16;
    std::array<uint8_t, 16> before{}, after{};
    const int af = family == NFPROTO_IPV4 ? AF_INET : AF_INET6;
    if (inet_pton(af, original, before.data()) != 1 ||
        inet_pton(af, target, after.data()) != 1) return EINVAL;

    auto batch = Message(NFNL_MSG_BATCH_BEGIN, kSeq, AF_UNSPEC);
    auto table = Message(kNewTable, kSeq + 1, family, NLM_F_CREATE | NLM_F_EXCL);
    TextAttr(&table, NFTA_TABLE_NAME, kTable);
    Append(&batch, table);

    auto chain = Message(kNewChain, kSeq + 2, family, NLM_F_CREATE | NLM_F_EXCL);
    TextAttr(&chain, NFTA_CHAIN_TABLE, kTable);
    TextAttr(&chain, NFTA_CHAIN_NAME, kChain);
    TextAttr(&chain, NFTA_CHAIN_TYPE, "nat");
    std::vector<uint8_t> hook;
    Be32(&hook, NFTA_HOOK_HOOKNUM, NF_INET_PRE_ROUTING);
    Be32(&hook, NFTA_HOOK_PRIORITY, static_cast<uint32_t>(-100));
    TopAttr(&chain, NFTA_CHAIN_HOOK | NLA_F_NESTED, hook.data(), hook.size());
    const uint32_t accept = htonl(NF_ACCEPT);
    TopAttr(&chain, NFTA_CHAIN_POLICY, &accept, sizeof(accept));
    Append(&batch, chain);

    std::vector<uint8_t> expressions, payload, cmp, value, immediate, nat;
    Be32(&payload, NFTA_PAYLOAD_DREG, NFT_REG_1);
    Be32(&payload, NFTA_PAYLOAD_BASE, NFT_PAYLOAD_NETWORK_HEADER);
    Be32(&payload, NFTA_PAYLOAD_OFFSET, family == NFPROTO_IPV4 ? 16 : 24);
    Be32(&payload, NFTA_PAYLOAD_LEN, address_size);
    Expression(&expressions, "payload", payload);
    Be32(&cmp, NFTA_CMP_SREG, NFT_REG_1);
    Be32(&cmp, NFTA_CMP_OP, NFT_CMP_EQ);
    Attr(&value, NFTA_DATA_VALUE, before.data(), address_size);
    Attr(&cmp, NFTA_CMP_DATA | NLA_F_NESTED, value.data(), value.size());
    Expression(&expressions, "cmp", cmp);
    Be32(&immediate, NFTA_IMMEDIATE_DREG, NFT_REG_1);
    value.clear();
    Attr(&value, NFTA_DATA_VALUE, after.data(), address_size);
    Attr(&immediate, NFTA_IMMEDIATE_DATA | NLA_F_NESTED, value.data(), value.size());
    Expression(&expressions, "immediate", immediate);
    Be32(&nat, NFTA_NAT_TYPE, NFT_NAT_DNAT);
    Be32(&nat, NFTA_NAT_FAMILY, family);
    Be32(&nat, NFTA_NAT_REG_ADDR_MIN, NFT_REG_1);
    Expression(&expressions, "nat", nat);
    auto rule = Message(kNewRule, kSeq + 3, family, NLM_F_CREATE | NLM_F_APPEND);
    TextAttr(&rule, NFTA_RULE_TABLE, kTable);
    TextAttr(&rule, NFTA_RULE_CHAIN, kChain);
    TopAttr(&rule, NFTA_RULE_EXPRESSIONS | NLA_F_NESTED,
            expressions.data(), expressions.size());
    Append(&batch, rule);
    Append(&batch, Message(NFNL_MSG_BATCH_END, kSeq + 4, AF_UNSPEC));

    const int fd = socket(AF_NETLINK, SOCK_RAW, NETLINK_NETFILTER);
    if (fd < 0) return errno;
    sockaddr_nl address{};
    address.nl_family = AF_NETLINK;
    timeval timeout{2, 0};
    int error = 0;
    if (bind(fd, reinterpret_cast<sockaddr*>(&address), sizeof(address)) < 0 ||
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) < 0) {
        error = errno;
    } else if (const ssize_t sent = sendto(fd, batch.data(), batch.size(), 0,
                                           reinterpret_cast<sockaddr*>(&address),
                                           sizeof(address));
               sent != static_cast<ssize_t>(batch.size())) {
        error = sent < 0 ? errno : EIO;
    } else {
        uint32_t seen = 0;
        while (seen != 0b111) {
            std::array<uint8_t, 4096> response{};
            const ssize_t size = recv(fd, response.data(), response.size(), 0);
            if (size < 0) { error = errno; break; }
            int remaining = static_cast<int>(size);
            for (auto* header = reinterpret_cast<nlmsghdr*>(response.data());
                 NLMSG_OK(header, remaining); header = NLMSG_NEXT(header, remaining)) {
                if (header->nlmsg_type != NLMSG_ERROR ||
                    header->nlmsg_len < NLMSG_LENGTH(sizeof(nlmsgerr)) ||
                    header->nlmsg_seq < kSeq || header->nlmsg_seq > kSeq + 3) continue;
                const auto* ack = reinterpret_cast<nlmsgerr*>(NLMSG_DATA(header));
                if (ack->error != 0) { error = -ack->error; break; }
                if (header->nlmsg_seq == kSeq) continue;
                seen |= 1u << (header->nlmsg_seq - kSeq - 1);
            }
            if (error) break;
        }
    }
    close(fd);
    return error;
}

}  // namespace forward_mtu_nft
