#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <fcntl.h>
#include <linux/capability.h>
#include <linux/if_ether.h>
#include <linux/if_packet.h>
#include <linux/netfilter.h>
#include <linux/netfilter/nf_tables.h>
#include <linux/netfilter/nf_tables_compat.h>
#include <linux/netfilter/nf_nat.h>
#include <linux/netfilter/nfnetlink.h>
#include <linux/netfilter/xt_addrtype.h>
#include <linux/netfilter/xt_tcpudp.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <net/if.h>
#include <poll.h>
#include <sched.h>
#include <sys/ioctl.h>
#include <sys/epoll.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <cerrno>
#include <chrono>
#include <cstdint>
#include <cstring>
#include <functional>
#include <initializer_list>
#include <string>
#include <tuple>
#include <utility>
#include <vector>

#include "rtnetlink_route_test_support.h"

namespace {
// Unknown subsystem avoids depending on whether host nftables modules are loaded.
constexpr uint16_t kUnknown = 255 << 8;
constexpr uint16_t kNftGetgen = (NFNL_SUBSYS_NFTABLES << 8) | NFT_MSG_GETGEN;
constexpr uint16_t kNftNewgen = (NFNL_SUBSYS_NFTABLES << 8) | NFT_MSG_NEWGEN;
constexpr uint16_t kNftGettable = (NFNL_SUBSYS_NFTABLES << 8) | NFT_MSG_GETTABLE;
constexpr uint16_t kNftGetchain = (NFNL_SUBSYS_NFTABLES << 8) | NFT_MSG_GETCHAIN;
constexpr uint16_t kNftGetset = (NFNL_SUBSYS_NFTABLES << 8) | NFT_MSG_GETSET;
constexpr uint16_t kNftGetsetelem = (NFNL_SUBSYS_NFTABLES << 8) | NFT_MSG_GETSETELEM;
constexpr uint16_t kNftGetflowtable = (NFNL_SUBSYS_NFTABLES << 8) | NFT_MSG_GETFLOWTABLE;
constexpr uint16_t kNftNewtable = (NFNL_SUBSYS_NFTABLES << 8) | NFT_MSG_NEWTABLE;
constexpr uint16_t kNftDeltable = (NFNL_SUBSYS_NFTABLES << 8) | NFT_MSG_DELTABLE;
constexpr uint16_t kNftNewchain = (NFNL_SUBSYS_NFTABLES << 8) | NFT_MSG_NEWCHAIN;
constexpr uint16_t kNftDelchain = (NFNL_SUBSYS_NFTABLES << 8) | NFT_MSG_DELCHAIN;
constexpr uint16_t kNftNewrule = (NFNL_SUBSYS_NFTABLES << 8) | NFT_MSG_NEWRULE;
constexpr uint16_t kNftGetrule = (NFNL_SUBSYS_NFTABLES << 8) | NFT_MSG_GETRULE;
constexpr uint16_t kNftDelrule = (NFNL_SUBSYS_NFTABLES << 8) | NFT_MSG_DELRULE;
constexpr uint16_t kNftNewset = (NFNL_SUBSYS_NFTABLES << 8) | NFT_MSG_NEWSET;
constexpr uint16_t kNftDelset = (NFNL_SUBSYS_NFTABLES << 8) | NFT_MSG_DELSET;
constexpr uint16_t kNftNewsetelem = (NFNL_SUBSYS_NFTABLES << 8) | NFT_MSG_NEWSETELEM;
constexpr uint16_t kNftDelsetelem = (NFNL_SUBSYS_NFTABLES << 8) | NFT_MSG_DELSETELEM;
constexpr uint16_t kNftCompatGet = (NFNL_SUBSYS_NFT_COMPAT << 8) | NFNL_MSG_COMPAT_GET;

class Fd {
 public:
  explicit Fd(int fd) : fd_(fd) {}
  ~Fd() { if (fd_ >= 0) close(fd_); }
  Fd(const Fd&) = delete;
  Fd& operator=(const Fd&) = delete;
  int get() const { return fd_; }
 private:
  int fd_;
};

int Open(int type = SOCK_RAW, int protocol = NETLINK_NETFILTER) {
  return socket(AF_NETLINK, type | SOCK_NONBLOCK | SOCK_CLOEXEC, protocol);
}

int Bind(int fd, uint32_t port = 0) {
  sockaddr_nl addr{};
  addr.nl_family = AF_NETLINK;
  addr.nl_pid = port;
  return bind(fd, reinterpret_cast<sockaddr*>(&addr), sizeof(addr));
}

uint32_t Port(int fd) {
  sockaddr_nl addr{};
  socklen_t size = sizeof(addr);
  if (getsockname(fd, reinterpret_cast<sockaddr*>(&addr), &size) < 0 ||
      size != sizeof(addr) || addr.nl_family != AF_NETLINK) return 0;
  return addr.nl_pid;
}

std::vector<uint8_t> Request(uint16_t type, uint32_t seq, size_t payload = sizeof(nfgenmsg)) {
  std::vector<uint8_t> bytes(NLMSG_SPACE(payload), 0);
  nlmsghdr h{};
  h.nlmsg_len = NLMSG_LENGTH(payload);
  h.nlmsg_type = type;
  h.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
  h.nlmsg_seq = seq;
  std::memcpy(bytes.data(), &h, sizeof(h));
  return bytes;
}

void AppendAttr(std::vector<uint8_t>* request, uint16_t type, const char* text) {
  const size_t value_len = std::strlen(text) + 1;
  const size_t attr_len = sizeof(nlattr) + value_len;
  const size_t offset = request->size();
  request->resize(offset + NLA_ALIGN(attr_len), 0);
  nlattr attr{};
  attr.nla_type = type;
  attr.nla_len = attr_len;
  std::memcpy(request->data() + offset, &attr, sizeof(attr));
  std::memcpy(request->data() + offset + sizeof(attr), text, value_len);
  nlmsghdr header{};
  std::memcpy(&header, request->data(), sizeof(header));
  header.nlmsg_len = offset + attr_len;
  std::memcpy(request->data(), &header, sizeof(header));
}

void AppendNla(std::vector<uint8_t>* bytes, uint16_t type,
               const uint8_t* value, size_t value_len) {
  const size_t attr_len = sizeof(nlattr) + value_len;
  const size_t offset = bytes->size();
  bytes->resize(offset + NLA_ALIGN(attr_len), 0);
  nlattr attr{};
  attr.nla_type = type;
  attr.nla_len = attr_len;
  std::memcpy(bytes->data() + offset, &attr, sizeof(attr));
  if (value_len) std::memcpy(bytes->data() + offset + sizeof(attr), value, value_len);
}

void AppendRawAttr(std::vector<uint8_t>* request, uint16_t type,
                   const uint8_t* value, size_t value_len) {
  const size_t offset = request->size();
  AppendNla(request, type, value, value_len);
  nlmsghdr header{};
  std::memcpy(&header, request->data(), sizeof(header));
  header.nlmsg_len = offset + sizeof(nlattr) + value_len;
  std::memcpy(request->data(), &header, sizeof(header));
}

void AppendBe32(std::vector<uint8_t>* request, uint16_t type, uint32_t value) {
  const uint32_t be = htonl(value);
  AppendRawAttr(request, type, reinterpret_cast<const uint8_t*>(&be), sizeof(be));
}

void AppendExpr(std::vector<uint8_t>* expressions, const char* name,
                const std::vector<uint8_t>& data) {
  std::vector<uint8_t> expression;
  AppendNla(&expression, NFTA_EXPR_NAME,
            reinterpret_cast<const uint8_t*>(name), std::strlen(name) + 1);
  AppendNla(&expression, NFTA_EXPR_DATA | NLA_F_NESTED, data.data(), data.size());
  AppendNla(expressions, NFTA_LIST_ELEM | NLA_F_NESTED,
            expression.data(), expression.size());
}

std::vector<uint8_t> RuleWithExpressions(uint32_t seq, const char* table,
                                         const char* chain,
                                         const std::vector<uint8_t>& expressions,
                                         uint8_t family = NFPROTO_IPV4) {
  auto rule = Request(kNftNewrule, seq);
  rule[NLMSG_HDRLEN] = family;
  nlmsghdr header{};
  std::memcpy(&header, rule.data(), sizeof(header));
  header.nlmsg_flags |= NLM_F_CREATE | NLM_F_APPEND;
  std::memcpy(rule.data(), &header, sizeof(header));
  AppendAttr(&rule, NFTA_RULE_TABLE, table);
  AppendAttr(&rule, NFTA_RULE_CHAIN, chain);
  AppendRawAttr(&rule, NFTA_RULE_EXPRESSIONS | NLA_F_NESTED,
                expressions.data(), expressions.size());
  return rule;
}

std::vector<uint8_t> MetaCounterRule(uint32_t seq, const char* table,
                                     const char* chain, uint32_t key,
                                     const uint8_t* expected, size_t length,
                                     uint8_t family = NFPROTO_IPV4) {
  std::vector<uint8_t> expressions;
  std::vector<uint8_t> meta;
  const uint32_t be_register = htonl(NFT_REG_1);
  const uint32_t be_key = htonl(key);
  AppendNla(&meta, NFTA_META_DREG,
            reinterpret_cast<const uint8_t*>(&be_register), 4);
  AppendNla(&meta, NFTA_META_KEY,
            reinterpret_cast<const uint8_t*>(&be_key), 4);
  AppendExpr(&expressions, "meta", meta);
  std::vector<uint8_t> comparison;
  const uint32_t equal = htonl(NFT_CMP_EQ);
  AppendNla(&comparison, NFTA_CMP_SREG,
            reinterpret_cast<const uint8_t*>(&be_register), 4);
  AppendNla(&comparison, NFTA_CMP_OP,
            reinterpret_cast<const uint8_t*>(&equal), 4);
  std::vector<uint8_t> value;
  AppendNla(&value, NFTA_DATA_VALUE, expected, length);
  AppendNla(&comparison, NFTA_CMP_DATA | NLA_F_NESTED,
            value.data(), value.size());
  AppendExpr(&expressions, "cmp", comparison);
  AppendExpr(&expressions, "counter", {});
  return RuleWithExpressions(seq, table, chain, expressions, family);
}

void AppendIpv4SourcePayload(std::vector<uint8_t>* expressions) {
  std::vector<uint8_t> payload;
  const uint32_t register_id = htonl(NFT_REG_1);
  const uint32_t base = htonl(NFT_PAYLOAD_NETWORK_HEADER);
  const uint32_t offset = htonl(12);
  const uint32_t length = htonl(4);
  AppendNla(&payload, NFTA_PAYLOAD_DREG,
            reinterpret_cast<const uint8_t*>(&register_id), 4);
  AppendNla(&payload, NFTA_PAYLOAD_BASE,
            reinterpret_cast<const uint8_t*>(&base), 4);
  AppendNla(&payload, NFTA_PAYLOAD_OFFSET,
            reinterpret_cast<const uint8_t*>(&offset), 4);
  AppendNla(&payload, NFTA_PAYLOAD_LEN,
            reinterpret_cast<const uint8_t*>(&length), 4);
  AppendExpr(expressions, "payload", payload);
}

std::vector<uint8_t> Ipv4SourceSetCounterRule(uint32_t seq, const char* table,
                                              const char* chain, const char* set) {
  std::vector<uint8_t> expressions;
  AppendIpv4SourcePayload(&expressions);

  std::vector<uint8_t> lookup;
  const uint32_t register_id = htonl(NFT_REG_1);
  AppendNla(&lookup, NFTA_LOOKUP_SET,
            reinterpret_cast<const uint8_t*>(set), std::strlen(set) + 1);
  AppendNla(&lookup, NFTA_LOOKUP_SREG,
            reinterpret_cast<const uint8_t*>(&register_id), 4);
  AppendExpr(&expressions, "lookup", lookup);
  AppendExpr(&expressions, "counter", {});
  return RuleWithExpressions(seq, table, chain, expressions);
}

std::vector<uint8_t> Ipv4SourceMapCounterRule(uint32_t seq, const char* table,
                                              const char* chain, const char* map,
                                              in_addr expected) {
  std::vector<uint8_t> expressions;
  AppendIpv4SourcePayload(&expressions);
  std::vector<uint8_t> lookup;
  const uint32_t source_register = htonl(NFT_REG_1);
  const uint32_t value_register = htonl(NFT_REG_2);
  AppendNla(&lookup, NFTA_LOOKUP_SET,
            reinterpret_cast<const uint8_t*>(map), std::strlen(map) + 1);
  AppendNla(&lookup, NFTA_LOOKUP_SREG,
            reinterpret_cast<const uint8_t*>(&source_register), 4);
  AppendNla(&lookup, NFTA_LOOKUP_DREG,
            reinterpret_cast<const uint8_t*>(&value_register), 4);
  AppendExpr(&expressions, "lookup", lookup);
  std::vector<uint8_t> data, comparison;
  const uint32_t equal = htonl(NFT_CMP_EQ);
  AppendNla(&data, NFTA_DATA_VALUE,
            reinterpret_cast<const uint8_t*>(&expected.s_addr), 4);
  AppendNla(&comparison, NFTA_CMP_SREG,
            reinterpret_cast<const uint8_t*>(&value_register), 4);
  AppendNla(&comparison, NFTA_CMP_OP,
            reinterpret_cast<const uint8_t*>(&equal), 4);
  AppendNla(&comparison, NFTA_CMP_DATA | NLA_F_NESTED, data.data(), data.size());
  AppendExpr(&expressions, "cmp", comparison);
  AppendExpr(&expressions, "counter", {});
  return RuleWithExpressions(seq, table, chain, expressions);
}

std::vector<uint8_t> PacketLengthCounterRule(uint32_t seq, const char* table,
                                              const char* chain) {
  std::vector<uint8_t> expressions, meta, byteorder, comparison, zero;
  const uint32_t register_id = htonl(NFT_REG_1);
  const uint32_t key = htonl(NFT_META_LEN);
  const uint32_t conversion = htonl(NFT_BYTEORDER_HTON);
  const uint32_t four_bytes = htonl(4);
  const uint32_t greater_than = htonl(NFT_CMP_GT);
  const uint32_t zero_value = 0;
  AppendNla(&meta, NFTA_META_DREG,
            reinterpret_cast<const uint8_t*>(&register_id), 4);
  AppendNla(&meta, NFTA_META_KEY, reinterpret_cast<const uint8_t*>(&key), 4);
  AppendExpr(&expressions, "meta", meta);
  AppendNla(&byteorder, NFTA_BYTEORDER_SREG,
            reinterpret_cast<const uint8_t*>(&register_id), 4);
  AppendNla(&byteorder, NFTA_BYTEORDER_DREG,
            reinterpret_cast<const uint8_t*>(&register_id), 4);
  AppendNla(&byteorder, NFTA_BYTEORDER_OP,
            reinterpret_cast<const uint8_t*>(&conversion), 4);
  AppendNla(&byteorder, NFTA_BYTEORDER_LEN,
            reinterpret_cast<const uint8_t*>(&four_bytes), 4);
  AppendNla(&byteorder, NFTA_BYTEORDER_SIZE,
            reinterpret_cast<const uint8_t*>(&four_bytes), 4);
  AppendExpr(&expressions, "byteorder", byteorder);
  AppendNla(&zero, NFTA_DATA_VALUE,
            reinterpret_cast<const uint8_t*>(&zero_value), 4);
  AppendNla(&comparison, NFTA_CMP_SREG,
            reinterpret_cast<const uint8_t*>(&register_id), 4);
  AppendNla(&comparison, NFTA_CMP_OP,
            reinterpret_cast<const uint8_t*>(&greater_than), 4);
  AppendNla(&comparison, NFTA_CMP_DATA | NLA_F_NESTED, zero.data(), zero.size());
  AppendExpr(&expressions, "cmp", comparison);
  AppendExpr(&expressions, "counter", {});
  return RuleWithExpressions(seq, table, chain, expressions);
}

std::vector<uint8_t> Ipv4SourceNotRangeCounterRule(uint32_t seq,
                                                    const char* table,
                                                    const char* chain,
                                                    in_addr first,
                                                    in_addr last) {
  std::vector<uint8_t> expressions, range, start, end;
  AppendIpv4SourcePayload(&expressions);
  const uint32_t register_id = htonl(NFT_REG_1);
  const uint32_t not_equal = htonl(NFT_RANGE_NEQ);
  AppendNla(&range, NFTA_RANGE_SREG,
            reinterpret_cast<const uint8_t*>(&register_id), 4);
  AppendNla(&range, NFTA_RANGE_OP,
            reinterpret_cast<const uint8_t*>(&not_equal), 4);
  AppendNla(&start, NFTA_DATA_VALUE,
            reinterpret_cast<const uint8_t*>(&first.s_addr), 4);
  AppendNla(&end, NFTA_DATA_VALUE,
            reinterpret_cast<const uint8_t*>(&last.s_addr), 4);
  AppendNla(&range, NFTA_RANGE_FROM_DATA | NLA_F_NESTED,
            start.data(), start.size());
  AppendNla(&range, NFTA_RANGE_TO_DATA | NLA_F_NESTED,
            end.data(), end.size());
  AppendExpr(&expressions, "range", range);
  AppendExpr(&expressions, "counter", {});
  return RuleWithExpressions(seq, table, chain, expressions);
}

std::vector<uint8_t> Ipv4SourceSet(uint32_t seq, const char* table,
                                   const char* set, uint32_t flags = 0) {
  auto request = Request(kNftNewset, seq);
  request[NLMSG_HDRLEN] = NFPROTO_IPV4;
  nlmsghdr header{};
  std::memcpy(&header, request.data(), sizeof(header));
  header.nlmsg_flags |= NLM_F_CREATE | NLM_F_EXCL;
  std::memcpy(request.data(), &header, sizeof(header));
  AppendAttr(&request, NFTA_SET_TABLE, table);
  AppendAttr(&request, NFTA_SET_NAME, set);
  AppendBe32(&request, NFTA_SET_FLAGS, flags);
  AppendBe32(&request, NFTA_SET_KEY_TYPE, 7);  // nftables ipv4_addr
  AppendBe32(&request, NFTA_SET_KEY_LEN, 4);
  if (flags & NFT_SET_MAP) {
    AppendBe32(&request, NFTA_SET_DATA_TYPE, 7);
    AppendBe32(&request, NFTA_SET_DATA_LEN, 4);
  }
  AppendBe32(&request, NFTA_SET_ID, 1);
  return request;
}

std::vector<uint8_t> Ipv4SourceSetElement(uint16_t type, uint32_t seq,
                                          const char* table, const char* set,
                                          in_addr address,
                                          const in_addr* mapped = nullptr,
                                          const in_addr* additional_key = nullptr) {
  auto request = Request(type, seq);
  request[NLMSG_HDRLEN] = NFPROTO_IPV4;
  if (type == kNftNewsetelem) {
    nlmsghdr header{};
    std::memcpy(&header, request.data(), sizeof(header));
    header.nlmsg_flags |= NLM_F_CREATE | NLM_F_EXCL;
    std::memcpy(request.data(), &header, sizeof(header));
  }
  AppendAttr(&request, NFTA_SET_ELEM_LIST_TABLE, table);
  AppendAttr(&request, NFTA_SET_ELEM_LIST_SET, set);
  std::vector<uint8_t> key, element, elements;
  AppendNla(&key, NFTA_DATA_VALUE,
            reinterpret_cast<const uint8_t*>(&address.s_addr), 4);
  AppendNla(&element, NFTA_SET_ELEM_KEY | NLA_F_NESTED, key.data(), key.size());
  if (mapped != nullptr) {
    std::vector<uint8_t> data;
    AppendNla(&data, NFTA_DATA_VALUE,
              reinterpret_cast<const uint8_t*>(&mapped->s_addr), 4);
    AppendNla(&element, NFTA_SET_ELEM_DATA | NLA_F_NESTED, data.data(), data.size());
  }
  AppendNla(&elements, NFTA_LIST_ELEM | NLA_F_NESTED, element.data(), element.size());
  if (additional_key != nullptr) {
    // libnftnl numbers elements within one NEWSETELEM message 1, 2, ... .
    std::vector<uint8_t> another_key, another_element;
    AppendNla(&another_key, NFTA_DATA_VALUE,
              reinterpret_cast<const uint8_t*>(&additional_key->s_addr), 4);
    AppendNla(&another_element, NFTA_SET_ELEM_KEY | NLA_F_NESTED,
              another_key.data(), another_key.size());
    AppendNla(&elements, (NFTA_LIST_ELEM + 1) | NLA_F_NESTED,
              another_element.data(), another_element.size());
  }
  AppendRawAttr(&request, NFTA_SET_ELEM_LIST_ELEMENTS | NLA_F_NESTED,
                elements.data(), elements.size());
  return request;
}

std::vector<uint8_t> CtStateCounterRule(uint32_t seq, const char* table,
                                        const char* chain, uint32_t state,
                                        uint8_t family = NFPROTO_IPV4) {
  std::vector<uint8_t> expressions;
  std::vector<uint8_t> ct;
  const uint32_t be_register = htonl(NFT_REG_1);
  const uint32_t be_key = htonl(NFT_CT_STATE);
  AppendNla(&ct, NFTA_CT_DREG, reinterpret_cast<const uint8_t*>(&be_register), 4);
  AppendNla(&ct, NFTA_CT_KEY, reinterpret_cast<const uint8_t*>(&be_key), 4);
  AppendExpr(&expressions, "ct", ct);
  std::vector<uint8_t> comparison;
  const uint32_t equal = htonl(NFT_CMP_EQ);
  AppendNla(&comparison, NFTA_CMP_SREG,
            reinterpret_cast<const uint8_t*>(&be_register), 4);
  AppendNla(&comparison, NFTA_CMP_OP,
            reinterpret_cast<const uint8_t*>(&equal), 4);
  std::vector<uint8_t> value;
  AppendNla(&value, NFTA_DATA_VALUE, reinterpret_cast<const uint8_t*>(&state), 4);
  AppendNla(&comparison, NFTA_CMP_DATA | NLA_F_NESTED, value.data(), value.size());
  AppendExpr(&expressions, "cmp", comparison);
  AppendExpr(&expressions, "counter", {});
  return RuleWithExpressions(seq, table, chain, expressions, family);
}

void AppendPayloadEqual(std::vector<uint8_t>* expressions, uint32_t base,
                        uint32_t offset, const uint8_t* expected, size_t length) {
  const uint32_t register_id = htonl(NFT_REG_1);
  const uint32_t be_base = htonl(base);
  const uint32_t be_offset = htonl(offset);
  const uint32_t be_length = htonl(static_cast<uint32_t>(length));
  std::vector<uint8_t> payload;
  AppendNla(&payload, NFTA_PAYLOAD_DREG,
            reinterpret_cast<const uint8_t*>(&register_id), 4);
  AppendNla(&payload, NFTA_PAYLOAD_BASE,
            reinterpret_cast<const uint8_t*>(&be_base), 4);
  AppendNla(&payload, NFTA_PAYLOAD_OFFSET,
            reinterpret_cast<const uint8_t*>(&be_offset), 4);
  AppendNla(&payload, NFTA_PAYLOAD_LEN,
            reinterpret_cast<const uint8_t*>(&be_length), 4);
  AppendExpr(expressions, "payload", payload);

  const uint32_t equal = htonl(NFT_CMP_EQ);
  std::vector<uint8_t> value, comparison;
  AppendNla(&value, NFTA_DATA_VALUE, expected, length);
  AppendNla(&comparison, NFTA_CMP_SREG,
            reinterpret_cast<const uint8_t*>(&register_id), 4);
  AppendNla(&comparison, NFTA_CMP_OP,
            reinterpret_cast<const uint8_t*>(&equal), 4);
  AppendNla(&comparison, NFTA_CMP_DATA | NLA_F_NESTED,
            value.data(), value.size());
  AppendExpr(expressions, "cmp", comparison);
}

void AppendImmediateBytes(std::vector<uint8_t>* expressions, uint32_t register_id,
                          const uint8_t* bytes, size_t length) {
  const uint32_t be_register = htonl(register_id);
  std::vector<uint8_t> value, immediate;
  AppendNla(&value, NFTA_DATA_VALUE, bytes, length);
  AppendNla(&immediate, NFTA_IMMEDIATE_DREG,
            reinterpret_cast<const uint8_t*>(&be_register), 4);
  AppendNla(&immediate, NFTA_IMMEDIATE_DATA | NLA_F_NESTED,
            value.data(), value.size());
  AppendExpr(expressions, "immediate", immediate);
}

std::vector<uint8_t> MetaMarkSetRule(uint32_t seq, const char* table,
                                     const char* chain, uint32_t mark) {
  std::vector<uint8_t> expressions, meta;
  AppendImmediateBytes(&expressions, NFT_REG_1,
                       reinterpret_cast<const uint8_t*>(&mark), sizeof(mark));
  const uint32_t register_id = htonl(NFT_REG_1);
  const uint32_t key = htonl(NFT_META_MARK);
  AppendNla(&meta, NFTA_META_SREG,
            reinterpret_cast<const uint8_t*>(&register_id), 4);
  AppendNla(&meta, NFTA_META_KEY, reinterpret_cast<const uint8_t*>(&key), 4);
  AppendExpr(&expressions, "meta", meta);
  return RuleWithExpressions(seq, table, chain, expressions);
}

std::vector<uint8_t> FibIpv4DestinationLocalCounterRule(uint32_t seq,
                                                        const char* table,
                                                        const char* chain) {
  std::vector<uint8_t> expressions, fib;
  const uint32_t register_id = htonl(NFT_REG_1);
  const uint32_t result = htonl(NFT_FIB_RESULT_ADDRTYPE);
  const uint32_t flags = htonl(NFTA_FIB_F_DADDR);
  AppendNla(&fib, NFTA_FIB_DREG,
            reinterpret_cast<const uint8_t*>(&register_id), 4);
  AppendNla(&fib, NFTA_FIB_RESULT,
            reinterpret_cast<const uint8_t*>(&result), 4);
  AppendNla(&fib, NFTA_FIB_FLAGS,
            reinterpret_cast<const uint8_t*>(&flags), 4);
  AppendExpr(&expressions, "fib", fib);
  const uint32_t local = RTN_LOCAL;
  const uint32_t equal = htonl(NFT_CMP_EQ);
  std::vector<uint8_t> value, comparison;
  AppendNla(&value, NFTA_DATA_VALUE,
            reinterpret_cast<const uint8_t*>(&local), sizeof(local));
  AppendNla(&comparison, NFTA_CMP_SREG,
            reinterpret_cast<const uint8_t*>(&register_id), 4);
  AppendNla(&comparison, NFTA_CMP_OP,
            reinterpret_cast<const uint8_t*>(&equal), 4);
  AppendNla(&comparison, NFTA_CMP_DATA | NLA_F_NESTED,
            value.data(), value.size());
  AppendExpr(&expressions, "cmp", comparison);
  AppendExpr(&expressions, "counter", {});
  return RuleWithExpressions(seq, table, chain, expressions);
}

std::vector<uint8_t> NativeIpv4DnatRule(uint32_t seq, const char* table,
                                        const char* chain, in_addr original,
                                        uint16_t original_port, in_addr target,
                                        uint16_t target_port) {
  std::vector<uint8_t> expressions;
  const uint16_t original_port_be = htons(original_port);
  const uint16_t target_port_be = htons(target_port);
  AppendPayloadEqual(&expressions, NFT_PAYLOAD_NETWORK_HEADER, 16,
                     reinterpret_cast<const uint8_t*>(&original.s_addr), 4);
  AppendPayloadEqual(&expressions, NFT_PAYLOAD_TRANSPORT_HEADER, 2,
                     reinterpret_cast<const uint8_t*>(&original_port_be), 2);
  AppendExpr(&expressions, "counter", {});
  AppendImmediateBytes(&expressions, NFT_REG_1,
                       reinterpret_cast<const uint8_t*>(&target.s_addr), 4);
  AppendImmediateBytes(&expressions, NFT_REG_2,
                       reinterpret_cast<const uint8_t*>(&target_port_be), 2);
  const uint32_t type = htonl(NFT_NAT_DNAT);
  const uint32_t family = htonl(NFPROTO_IPV4);
  const uint32_t address_register = htonl(NFT_REG_1);
  const uint32_t port_register = htonl(NFT_REG_2);
  std::vector<uint8_t> nat;
  AppendNla(&nat, NFTA_NAT_TYPE, reinterpret_cast<const uint8_t*>(&type), 4);
  AppendNla(&nat, NFTA_NAT_FAMILY, reinterpret_cast<const uint8_t*>(&family), 4);
  AppendNla(&nat, NFTA_NAT_REG_ADDR_MIN,
            reinterpret_cast<const uint8_t*>(&address_register), 4);
  AppendNla(&nat, NFTA_NAT_REG_PROTO_MIN,
            reinterpret_cast<const uint8_t*>(&port_register), 4);
  AppendExpr(&expressions, "nat", nat);
  return RuleWithExpressions(seq, table, chain, expressions);
}

std::vector<uint8_t> NativeIpv4RedirectRule(uint32_t seq, const char* table,
                                            const char* chain, in_addr original,
                                            uint16_t original_port,
                                            uint16_t target_port) {
  std::vector<uint8_t> expressions;
  const uint16_t original_port_be = htons(original_port);
  const uint16_t target_port_be = htons(target_port);
  AppendPayloadEqual(&expressions, NFT_PAYLOAD_NETWORK_HEADER, 16,
                     reinterpret_cast<const uint8_t*>(&original.s_addr), 4);
  AppendPayloadEqual(&expressions, NFT_PAYLOAD_TRANSPORT_HEADER, 2,
                     reinterpret_cast<const uint8_t*>(&original_port_be), 2);
  AppendExpr(&expressions, "counter", {});
  AppendImmediateBytes(&expressions, NFT_REG_1,
                       reinterpret_cast<const uint8_t*>(&target_port_be), 2);
  std::vector<uint8_t> redirect;
  const uint32_t port_register = htonl(NFT_REG_1);
  AppendNla(&redirect, NFTA_REDIR_REG_PROTO_MIN,
            reinterpret_cast<const uint8_t*>(&port_register), 4);
  AppendExpr(&expressions, "redir", redirect);
  return RuleWithExpressions(seq, table, chain, expressions);
}

std::vector<uint8_t> NativeIpv4SnatRule(uint32_t seq, const char* table,
                                        const char* chain, in_addr original,
                                        in_addr target) {
  std::vector<uint8_t> expressions;
  AppendPayloadEqual(&expressions, NFT_PAYLOAD_NETWORK_HEADER, 12,
                     reinterpret_cast<const uint8_t*>(&original.s_addr), 4);
  AppendExpr(&expressions, "counter", {});
  AppendImmediateBytes(&expressions, NFT_REG_1,
                       reinterpret_cast<const uint8_t*>(&target.s_addr), 4);
  const uint32_t type = htonl(NFT_NAT_SNAT);
  const uint32_t family = htonl(NFPROTO_IPV4);
  const uint32_t address_register = htonl(NFT_REG_1);
  std::vector<uint8_t> nat;
  AppendNla(&nat, NFTA_NAT_TYPE, reinterpret_cast<const uint8_t*>(&type), 4);
  AppendNla(&nat, NFTA_NAT_FAMILY, reinterpret_cast<const uint8_t*>(&family), 4);
  AppendNla(&nat, NFTA_NAT_REG_ADDR_MIN,
            reinterpret_cast<const uint8_t*>(&address_register), 4);
  AppendExpr(&expressions, "nat", nat);
  return RuleWithExpressions(seq, table, chain, expressions);
}

std::vector<uint8_t> XtIpv4DnatRule(uint32_t seq, const char* table,
                                    const char* chain, in_addr original,
                                    uint16_t original_port, in_addr target,
                                    uint16_t target_port) {
  std::vector<uint8_t> expressions;
  const uint16_t original_port_be = htons(original_port);
  AppendPayloadEqual(&expressions, NFT_PAYLOAD_NETWORK_HEADER, 16,
                     reinterpret_cast<const uint8_t*>(&original.s_addr), 4);
  AppendPayloadEqual(&expressions, NFT_PAYLOAD_TRANSPORT_HEADER, 2,
                     reinterpret_cast<const uint8_t*>(&original_port_be), 2);
  nf_nat_ipv4_multi_range_compat range{};
  range.rangesize = 1;
  range.range[0].flags = NF_NAT_RANGE_MAP_IPS | NF_NAT_RANGE_PROTO_SPECIFIED;
  range.range[0].min_ip = range.range[0].max_ip = target.s_addr;
  range.range[0].min.tcp.port = range.range[0].max.tcp.port = htons(target_port);
  std::vector<uint8_t> xt;
  AppendNla(&xt, NFTA_TARGET_NAME,
            reinterpret_cast<const uint8_t*>("DNAT"), sizeof("DNAT"));
  const uint32_t revision = htonl(0);
  AppendNla(&xt, NFTA_TARGET_REV,
            reinterpret_cast<const uint8_t*>(&revision), sizeof(revision));
  AppendNla(&xt, NFTA_TARGET_INFO,
            reinterpret_cast<const uint8_t*>(&range), sizeof(range));
  AppendExpr(&expressions, "target", xt);
  auto rule = RuleWithExpressions(seq, table, chain, expressions);
  std::vector<uint8_t> compat;
  const uint32_t protocol = htonl(IPPROTO_UDP), flags = 0;
  AppendNla(&compat, NFTA_RULE_COMPAT_PROTO,
            reinterpret_cast<const uint8_t*>(&protocol), sizeof(protocol));
  AppendNla(&compat, NFTA_RULE_COMPAT_FLAGS,
            reinterpret_cast<const uint8_t*>(&flags), sizeof(flags));
  AppendRawAttr(&rule, NFTA_RULE_COMPAT | NLA_F_NESTED,
                compat.data(), compat.size());
  return rule;
}

std::vector<uint8_t> Ipv6AddrtypeLocalDropRule(uint32_t seq, const char* table,
                                               const char* chain) {
  xt_addrtype_info_v1 addrtype{};
  addrtype.dest = XT_ADDRTYPE_LOCAL;
  std::vector<uint8_t> match;
  AppendNla(&match, NFTA_MATCH_NAME,
            reinterpret_cast<const uint8_t*>("addrtype"), sizeof("addrtype"));
  const uint32_t revision = htonl(1);
  AppendNla(&match, NFTA_MATCH_REV,
            reinterpret_cast<const uint8_t*>(&revision), sizeof(revision));
  AppendNla(&match, NFTA_MATCH_INFO,
            reinterpret_cast<const uint8_t*>(&addrtype), sizeof(addrtype));
  std::vector<uint8_t> expressions;
  AppendExpr(&expressions, "match", match);
  AppendExpr(&expressions, "counter", {});
  const uint32_t drop = htonl(NF_DROP);
  std::vector<uint8_t> verdict, data, immediate;
  AppendNla(&verdict, NFTA_VERDICT_CODE,
            reinterpret_cast<const uint8_t*>(&drop), sizeof(drop));
  AppendNla(&data, NFTA_DATA_VERDICT | NLA_F_NESTED,
            verdict.data(), verdict.size());
  const uint32_t verdict_reg = htonl(NFT_REG_VERDICT);
  AppendNla(&immediate, NFTA_IMMEDIATE_DREG,
            reinterpret_cast<const uint8_t*>(&verdict_reg), sizeof(verdict_reg));
  AppendNla(&immediate, NFTA_IMMEDIATE_DATA | NLA_F_NESTED,
            data.data(), data.size());
  AppendExpr(&expressions, "immediate", immediate);
  return RuleWithExpressions(seq, table, chain, expressions, NFPROTO_IPV6);
}

std::vector<uint8_t> ProtocolCounterRule(uint32_t seq, const char* table,
                                         const char* chain, uint32_t op,
                                         uint32_t payload_base = NFT_PAYLOAD_NETWORK_HEADER,
                                         uint32_t register_id = NFT_REG_1,
                                         uint32_t payload_offset = 9,
                                         uint32_t compare_register = UINT32_MAX,
                                         bool bitwise = false,
                                         uint32_t bitwise_shift = 1,
                                         bool drop = true,
                                         const in_addr* destination = nullptr,
                                         uint8_t protocol = IPPROTO_UDP,
                                         uint8_t family = NFPROTO_IPV4) {
  std::vector<uint8_t> expressions;
  std::vector<uint8_t> payload;
  const uint32_t be_register = htonl(register_id);
  const uint32_t be_base = htonl(payload_base);
  const uint32_t be_offset = htonl(payload_offset);
  const uint32_t be_length = htonl(1);
  AppendNla(&payload, NFTA_PAYLOAD_DREG,
            reinterpret_cast<const uint8_t*>(&be_register), 4);
  AppendNla(&payload, NFTA_PAYLOAD_BASE,
            reinterpret_cast<const uint8_t*>(&be_base), 4);
  AppendNla(&payload, NFTA_PAYLOAD_OFFSET,
            reinterpret_cast<const uint8_t*>(&be_offset), 4);
  AppendNla(&payload, NFTA_PAYLOAD_LEN,
            reinterpret_cast<const uint8_t*>(&be_length), 4);
  AppendExpr(&expressions, "payload", payload);

  if (bitwise) {
    auto append_bitwise = [&](uint32_t source, uint32_t destination, uint32_t operation,
                              const uint8_t* mask, const uint8_t* xor_value,
                              const uint32_t* shift) {
      std::vector<uint8_t> attrs;
      AppendNla(&attrs, NFTA_BITWISE_SREG,
                reinterpret_cast<const uint8_t*>(&source), 4);
      AppendNla(&attrs, NFTA_BITWISE_DREG,
                reinterpret_cast<const uint8_t*>(&destination), 4);
      const uint32_t be_len = htonl(1);
      AppendNla(&attrs, NFTA_BITWISE_LEN,
                reinterpret_cast<const uint8_t*>(&be_len), 4);
      if (mask != nullptr) {
        std::vector<uint8_t> nested;
        AppendNla(&nested, NFTA_DATA_VALUE, mask, 1);
        AppendNla(&attrs, NFTA_BITWISE_MASK | NLA_F_NESTED,
                  nested.data(), nested.size());
        nested.clear();
        AppendNla(&nested, NFTA_DATA_VALUE, xor_value, 1);
        AppendNla(&attrs, NFTA_BITWISE_XOR | NLA_F_NESTED,
                  nested.data(), nested.size());
      } else {
        const uint32_t be_op = htonl(operation);
        AppendNla(&attrs, NFTA_BITWISE_OP,
                  reinterpret_cast<const uint8_t*>(&be_op), 4);
        std::vector<uint8_t> nested;
        AppendNla(&nested, NFTA_DATA_VALUE,
                  reinterpret_cast<const uint8_t*>(shift), 4);
        AppendNla(&attrs, NFTA_BITWISE_DATA | NLA_F_NESTED,
                  nested.data(), nested.size());
      }
      AppendExpr(&expressions, "bitwise", attrs);
    };
    const uint8_t mask = 0x1f;
    const uint8_t xor_value = 1;
    const uint32_t source = htonl(register_id);
    const uint32_t destination = htonl(NFT_REG_2);
    append_bitwise(source, destination, NFT_BITWISE_BOOL, &mask, &xor_value, nullptr);
    append_bitwise(destination, destination, NFT_BITWISE_LSHIFT, nullptr, nullptr,
                   &bitwise_shift);
    append_bitwise(destination, destination, NFT_BITWISE_RSHIFT, nullptr, nullptr,
                   &bitwise_shift);
  }

  std::vector<uint8_t> comparison;
  const uint32_t be_op = htonl(op);
  const uint32_t be_compare_register = htonl(
      compare_register == UINT32_MAX ? (bitwise ? NFT_REG_2 : register_id)
                                     : compare_register);
  AppendNla(&comparison, NFTA_CMP_SREG,
            reinterpret_cast<const uint8_t*>(&be_compare_register), 4);
  AppendNla(&comparison, NFTA_CMP_OP,
            reinterpret_cast<const uint8_t*>(&be_op), 4);
  std::vector<uint8_t> value;
  const uint8_t matched_protocol = bitwise ? (protocol ^ 1) : protocol;
  AppendNla(&value, NFTA_DATA_VALUE, &matched_protocol, 1);
  AppendNla(&comparison, NFTA_CMP_DATA | NLA_F_NESTED,
            value.data(), value.size());
  AppendExpr(&expressions, "cmp", comparison);

  if (destination != nullptr) {
    std::vector<uint8_t> destination_payload;
    const uint32_t network_base = htonl(NFT_PAYLOAD_NETWORK_HEADER);
    const uint32_t be_offset = htonl(16);
    const uint32_t be_length = htonl(4);
    AppendNla(&destination_payload, NFTA_PAYLOAD_DREG,
              reinterpret_cast<const uint8_t*>(&be_register), 4);
    AppendNla(&destination_payload, NFTA_PAYLOAD_BASE,
              reinterpret_cast<const uint8_t*>(&network_base), 4);
    AppendNla(&destination_payload, NFTA_PAYLOAD_OFFSET,
              reinterpret_cast<const uint8_t*>(&be_offset), 4);
    AppendNla(&destination_payload, NFTA_PAYLOAD_LEN,
              reinterpret_cast<const uint8_t*>(&be_length), 4);
    AppendExpr(&expressions, "payload", destination_payload);
    std::vector<uint8_t> destination_cmp;
    const uint32_t equal = htonl(NFT_CMP_EQ);
    AppendNla(&destination_cmp, NFTA_CMP_SREG,
              reinterpret_cast<const uint8_t*>(&be_register), 4);
    AppendNla(&destination_cmp, NFTA_CMP_OP,
              reinterpret_cast<const uint8_t*>(&equal), 4);
    std::vector<uint8_t> address_value;
    AppendNla(&address_value, NFTA_DATA_VALUE,
              reinterpret_cast<const uint8_t*>(&destination->s_addr), 4);
    AppendNla(&destination_cmp, NFTA_CMP_DATA | NLA_F_NESTED,
              address_value.data(), address_value.size());
    AppendExpr(&expressions, "cmp", destination_cmp);
  }

  AppendExpr(&expressions, "counter", {});
  if (drop) {
    std::vector<uint8_t> verdict;
    const uint32_t drop_code = htonl(NF_DROP);
    AppendNla(&verdict, NFTA_VERDICT_CODE,
              reinterpret_cast<const uint8_t*>(&drop_code), 4);
    std::vector<uint8_t> data;
    AppendNla(&data, NFTA_DATA_VERDICT | NLA_F_NESTED,
              verdict.data(), verdict.size());
    std::vector<uint8_t> immediate;
    const uint32_t verdict_register = htonl(NFT_REG_VERDICT);
    AppendNla(&immediate, NFTA_IMMEDIATE_DREG,
              reinterpret_cast<const uint8_t*>(&verdict_register), 4);
    AppendNla(&immediate, NFTA_IMMEDIATE_DATA | NLA_F_NESTED,
              data.data(), data.size());
    AppendExpr(&expressions, "immediate", immediate);
  }
  return RuleWithExpressions(seq, table, chain, expressions, family);
}

std::vector<uint8_t> AliasedBitwiseRule(uint32_t seq, const char* table,
                                        const char* chain,
                                        uint32_t operation = NFT_BITWISE_BOOL) {
  std::vector<uint8_t> expressions;
  std::vector<uint8_t> payload;
  const uint32_t be_source = htonl(NFT_REG32_00);
  const uint32_t be_destination = htonl(NFT_REG32_01);
  const uint32_t be_result = htonl(NFT_REG32_02);
  const uint32_t be_base = htonl(NFT_PAYLOAD_NETWORK_HEADER);
  const uint32_t be_offset = htonl(0);
  const uint32_t be_len = htonl(8);
  AppendNla(&payload, NFTA_PAYLOAD_DREG, reinterpret_cast<const uint8_t*>(&be_source), 4);
  AppendNla(&payload, NFTA_PAYLOAD_BASE, reinterpret_cast<const uint8_t*>(&be_base), 4);
  AppendNla(&payload, NFTA_PAYLOAD_OFFSET, reinterpret_cast<const uint8_t*>(&be_offset), 4);
  AppendNla(&payload, NFTA_PAYLOAD_LEN, reinterpret_cast<const uint8_t*>(&be_len), 4);
  AppendExpr(&expressions, "payload", payload);

  std::vector<uint8_t> bitwise;
  AppendNla(&bitwise, NFTA_BITWISE_SREG, reinterpret_cast<const uint8_t*>(&be_source), 4);
  AppendNla(&bitwise, NFTA_BITWISE_DREG, reinterpret_cast<const uint8_t*>(&be_destination), 4);
  AppendNla(&bitwise, NFTA_BITWISE_LEN, reinterpret_cast<const uint8_t*>(&be_len), 4);
  const uint32_t be_operation = htonl(operation);
  AppendNla(&bitwise, NFTA_BITWISE_OP,
            reinterpret_cast<const uint8_t*>(&be_operation), 4);
  std::array<uint8_t, 8> mask{};
  mask.fill(0xff);
  const std::array<uint8_t, 8> xor_value{};
  std::vector<uint8_t> nested;
  AppendNla(&nested, NFTA_DATA_VALUE, mask.data(), mask.size());
  AppendNla(&bitwise, NFTA_BITWISE_MASK | NLA_F_NESTED, nested.data(), nested.size());
  nested.clear();
  AppendNla(&nested, NFTA_DATA_VALUE, xor_value.data(), xor_value.size());
  AppendNla(&bitwise, NFTA_BITWISE_XOR | NLA_F_NESTED, nested.data(), nested.size());
  AppendExpr(&expressions, "bitwise", bitwise);

  std::vector<uint8_t> comparison;
  const uint32_t be_op = htonl(NFT_CMP_EQ);
  AppendNla(&comparison, NFTA_CMP_SREG, reinterpret_cast<const uint8_t*>(&be_result), 4);
  AppendNla(&comparison, NFTA_CMP_OP, reinterpret_cast<const uint8_t*>(&be_op), 4);
  const uint8_t expected[] = {0x45, 0, 0, 29};
  nested.clear();
  AppendNla(&nested, NFTA_DATA_VALUE, expected, sizeof(expected));
  AppendNla(&comparison, NFTA_CMP_DATA | NLA_F_NESTED, nested.data(), nested.size());
  AppendExpr(&expressions, "cmp", comparison);

  AppendExpr(&expressions, "counter", {});
  std::vector<uint8_t> verdict;
  const uint32_t drop = htonl(NF_DROP);
  AppendNla(&verdict, NFTA_VERDICT_CODE, reinterpret_cast<const uint8_t*>(&drop), 4);
  nested.clear();
  AppendNla(&nested, NFTA_DATA_VERDICT | NLA_F_NESTED,
            verdict.data(), verdict.size());
  std::vector<uint8_t> immediate;
  const uint32_t be_verdict_register = htonl(NFT_REG_VERDICT);
  AppendNla(&immediate, NFTA_IMMEDIATE_DREG,
            reinterpret_cast<const uint8_t*>(&be_verdict_register), 4);
  AppendNla(&immediate, NFTA_IMMEDIATE_DATA | NLA_F_NESTED,
            nested.data(), nested.size());
  AppendExpr(&expressions, "immediate", immediate);
  return RuleWithExpressions(seq, table, chain, expressions);
}

std::vector<uint8_t> ImmediateRule(uint32_t seq, const char* table,
                                   const char* chain, uint32_t verdict,
                                   bool append, const char* target = nullptr) {
  auto rule = Request(kNftNewrule, seq);
  rule[NLMSG_HDRLEN] = NFPROTO_IPV4;
  nlmsghdr header{};
  std::memcpy(&header, rule.data(), sizeof(header));
  header.nlmsg_flags |= NLM_F_CREATE | (append ? NLM_F_APPEND : 0);
  std::memcpy(rule.data(), &header, sizeof(header));
  AppendAttr(&rule, NFTA_RULE_TABLE, table);
  AppendAttr(&rule, NFTA_RULE_CHAIN, chain);
  std::vector<uint8_t> verdict_data;
  const uint32_t code = htonl(verdict);
  AppendNla(&verdict_data, NFTA_VERDICT_CODE,
            reinterpret_cast<const uint8_t*>(&code), sizeof(code));
  if (target != nullptr) {
    AppendNla(&verdict_data, NFTA_VERDICT_CHAIN,
              reinterpret_cast<const uint8_t*>(target), std::strlen(target) + 1);
  }
  std::vector<uint8_t> data;
  AppendNla(&data, NFTA_DATA_VERDICT | NLA_F_NESTED,
            verdict_data.data(), verdict_data.size());
  std::vector<uint8_t> immediate;
  const uint32_t verdict_register = htonl(NFT_REG_VERDICT);
  AppendNla(&immediate, NFTA_IMMEDIATE_DREG,
            reinterpret_cast<const uint8_t*>(&verdict_register), sizeof(verdict_register));
  AppendNla(&immediate, NFTA_IMMEDIATE_DATA | NLA_F_NESTED,
            data.data(), data.size());
  std::vector<uint8_t> expression;
  const char expr_name[] = "immediate";
  AppendNla(&expression, NFTA_EXPR_NAME,
            reinterpret_cast<const uint8_t*>(expr_name), sizeof(expr_name));
  AppendNla(&expression, NFTA_EXPR_DATA | NLA_F_NESTED,
            immediate.data(), immediate.size());
  std::vector<uint8_t> expressions;
  AppendNla(&expressions, NFTA_LIST_ELEM | NLA_F_NESTED,
            expression.data(), expression.size());
  AppendRawAttr(&rule, NFTA_RULE_EXPRESSIONS | NLA_F_NESTED,
                expressions.data(), expressions.size());
  return rule;
}

std::vector<uint8_t> TableBatch(uint16_t mutation, uint32_t seq, const char* name,
                                uint8_t family = NFPROTO_IPV4) {
  auto begin = Request(NFNL_MSG_BATCH_BEGIN, seq);
  nfgenmsg begin_gen{};
  begin_gen.res_id = htons(NFNL_SUBSYS_NFTABLES);
  std::memcpy(begin.data() + NLMSG_HDRLEN, &begin_gen, sizeof(begin_gen));
  auto change = Request(mutation, seq + 1);
  change[NLMSG_HDRLEN] = family;
  nlmsghdr header{};
  std::memcpy(&header, change.data(), sizeof(header));
  header.nlmsg_flags |= NLM_F_CREATE | NLM_F_EXCL;
  std::memcpy(change.data(), &header, sizeof(header));
  AppendAttr(&change, NFTA_TABLE_NAME, name);
  auto end = Request(NFNL_MSG_BATCH_END, seq + 2);
  begin.insert(begin.end(), change.begin(), change.end());
  begin.insert(begin.end(), end.begin(), end.end());
  return begin;
}

std::vector<uint8_t> NewChain(uint32_t seq, const char* table,
                              const char* name, bool prerouting = false,
                              uint32_t policy = NF_ACCEPT,
                              uint32_t hook_number = NF_INET_PRE_ROUTING,
                              uint8_t family = NFPROTO_IPV4,
                              int32_t priority_value = 0,
                              const char* chain_type = "filter") {
  auto chain = Request(kNftNewchain, seq);
  chain[NLMSG_HDRLEN] = family;
  nlmsghdr header{};
  std::memcpy(&header, chain.data(), sizeof(header));
  header.nlmsg_flags |= NLM_F_CREATE | NLM_F_EXCL;
  std::memcpy(chain.data(), &header, sizeof(header));
  AppendAttr(&chain, NFTA_CHAIN_TABLE, table);
  AppendAttr(&chain, NFTA_CHAIN_NAME, name);
  if (prerouting) {
    AppendAttr(&chain, NFTA_CHAIN_TYPE, chain_type);
    std::vector<uint8_t> hook;
    const uint32_t hooknum = htonl(hook_number);
    const uint32_t priority = htonl(static_cast<uint32_t>(priority_value));
    AppendNla(&hook, NFTA_HOOK_HOOKNUM,
              reinterpret_cast<const uint8_t*>(&hooknum), sizeof(hooknum));
    AppendNla(&hook, NFTA_HOOK_PRIORITY,
              reinterpret_cast<const uint8_t*>(&priority), sizeof(priority));
    AppendRawAttr(&chain, NFTA_CHAIN_HOOK | NLA_F_NESTED, hook.data(), hook.size());
    AppendBe32(&chain, NFTA_CHAIN_POLICY, policy);
  }
  return chain;
}

std::array<uint8_t, 28> Ipv4UdpProbe(uint32_t source, uint32_t destination,
                                     uint16_t identification) {
  std::array<uint8_t, 28> packet{};
  packet[0] = 0x45;
  packet[2] = 0;
  packet[3] = static_cast<uint8_t>(packet.size());
  packet[4] = static_cast<uint8_t>(identification >> 8);
  packet[5] = static_cast<uint8_t>(identification);
  packet[8] = 64;
  packet[9] = IPPROTO_UDP;
  std::memcpy(packet.data() + 12, &source, sizeof(source));
  std::memcpy(packet.data() + 16, &destination, sizeof(destination));
  packet[20] = 0x30;
  packet[21] = 0x39;
  packet[22] = 0x30;
  packet[23] = 0x3a;
  packet[25] = 8;
  uint32_t sum = 0;
  for (size_t offset = 0; offset < 20; offset += 2) {
    sum += (static_cast<uint16_t>(packet[offset]) << 8) | packet[offset + 1];
  }
  while (sum >> 16) sum = (sum & 0xffff) + (sum >> 16);
  const uint16_t checksum = static_cast<uint16_t>(~sum);
  packet[10] = static_cast<uint8_t>(checksum >> 8);
  packet[11] = static_cast<uint8_t>(checksum);
  return packet;
}

template <size_t N>
bool InjectVethIpProbe(int fd, unsigned ifindex, const char* peer_name,
                       const std::array<uint8_t, N>& packet,
                       uint16_t ether_type = ETH_P_IP) {
  ifreq request{};
  std::strncpy(request.ifr_name, peer_name, IFNAMSIZ - 1);
  if (ioctl(fd, SIOCGIFHWADDR, &request) != 0) return false;
  sockaddr_ll destination{};
  destination.sll_family = AF_PACKET;
  destination.sll_protocol = htons(ether_type);
  destination.sll_ifindex = static_cast<int>(ifindex);
  destination.sll_halen = ETH_ALEN;
  std::memcpy(destination.sll_addr, request.ifr_hwaddr.sa_data, ETH_ALEN);
  return sendto(fd, packet.data(), packet.size(), 0,
                reinterpret_cast<sockaddr*>(&destination), sizeof(destination)) ==
         static_cast<ssize_t>(packet.size());
}

std::array<uint8_t, 40> Ipv6NoNextProbe(const char* source, const char* destination) {
  std::array<uint8_t, 40> packet{};
  packet[0] = 0x60;
  packet[6] = IPPROTO_NONE;
  packet[7] = 64;
  EXPECT_EQ(1, inet_pton(AF_INET6, source, packet.data() + 8));
  EXPECT_EQ(1, inet_pton(AF_INET6, destination, packet.data() + 24));
  return packet;
}

std::array<uint8_t, 56> Ipv6FirstFragmentProbe(const char* source,
                                                const char* destination) {
  std::array<uint8_t, 56> packet{};
  packet[0] = 0x60;
  packet[5] = 16;
  packet[6] = 44;
  packet[7] = 64;
  EXPECT_EQ(1, inet_pton(AF_INET6, source, packet.data() + 8));
  EXPECT_EQ(1, inet_pton(AF_INET6, destination, packet.data() + 24));
  packet[40] = IPPROTO_UDP;
  packet[43] = 1;  // Offset zero, more fragments follow.
  packet[47] = 0x35;
  packet[48] = 0x30;
  packet[49] = 0x39;
  packet[50] = 0x5b;
  packet[51] = 0xa0;
  packet[53] = 16;
  packet[54] = 0x12;
  packet[55] = 0x34;
  return packet;
}

bool SawNeighborSolicitTarget(int fd, const char* target, int timeout_ms) {
  in6_addr expected{};
  if (inet_pton(AF_INET6, target, &expected) != 1) return false;
  const auto deadline = std::chrono::steady_clock::now() +
                        std::chrono::milliseconds(timeout_ms);
  while (std::chrono::steady_clock::now() < deadline) {
    const auto remaining = std::chrono::duration_cast<std::chrono::milliseconds>(
        deadline - std::chrono::steady_clock::now());
    pollfd ready{fd, POLLIN, 0};
    if (poll(&ready, 1, static_cast<int>(remaining.count())) <= 0) return false;
    std::array<uint8_t, 160> frame{};
    sockaddr_ll source{};
    socklen_t source_len = sizeof(source);
    const ssize_t count = recvfrom(fd, frame.data(), frame.size(), 0,
                                   reinterpret_cast<sockaddr*>(&source), &source_len);
    if (source.sll_pkttype == PACKET_OUTGOING || count < 14 + 40 + 24) continue;
    if (frame[12] != 0x86 || frame[13] != 0xdd || frame[14 + 6] != IPPROTO_ICMPV6 ||
        frame[14 + 40] != 135) continue;
    if (std::memcmp(frame.data() + 14 + 40 + 8, &expected, sizeof(expected)) == 0)
      return true;
  }
  return false;
}

bool SawArpTarget(int fd, uint32_t target, int timeout_ms) {
  const auto deadline = std::chrono::steady_clock::now() +
                        std::chrono::milliseconds(timeout_ms);
  while (std::chrono::steady_clock::now() < deadline) {
    const auto remaining = std::chrono::duration_cast<std::chrono::milliseconds>(
        deadline - std::chrono::steady_clock::now());
    pollfd ready{fd, POLLIN, 0};
    if (poll(&ready, 1, static_cast<int>(remaining.count())) <= 0) return false;
    std::array<uint8_t, 128> frame{};
    sockaddr_ll source{};
    socklen_t source_len = sizeof(source);
    const ssize_t count = recvfrom(fd, frame.data(), frame.size(), 0,
                                   reinterpret_cast<sockaddr*>(&source), &source_len);
    if (count < 42) continue;
    uint32_t request_target = 0;
    std::memcpy(&request_target, frame.data() + 14 + 24, sizeof(request_target));
    if (source.sll_pkttype == PACKET_OUTGOING) continue;
    if (request_target == target) return true;
  }
  return false;
}

std::vector<uint8_t> JumpDepthBatch(uint32_t seq, const char* table_name,
                                    uint32_t edges) {
  auto batch = TableBatch(kNftNewtable, seq, table_name);
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto base = NewChain(seq + 2, table_name, "prerouting", true);
  batch.insert(batch.end(), base.begin(), base.end());
  for (uint32_t index = 0; index < edges; ++index) {
    const std::string name = "c" + std::to_string(index);
    auto chain = NewChain(seq + 3 + index, table_name, name.c_str());
    batch.insert(batch.end(), chain.begin(), chain.end());
  }
  for (uint32_t index = 0; index < edges; ++index) {
    const std::string source = index == 0 ? "prerouting" : "c" + std::to_string(index - 1);
    const std::string target = "c" + std::to_string(index);
    auto rule = ImmediateRule(seq + 3 + edges + index, table_name, source.c_str(),
                              static_cast<uint32_t>(NFT_JUMP), true, target.c_str());
    batch.insert(batch.end(), rule.begin(), rule.end());
  }
  auto end = Request(NFNL_MSG_BATCH_END, seq + 3 + 2 * edges);
  batch.insert(batch.end(), end.begin(), end.end());
  return batch;
}

ssize_t Send(int fd, const std::vector<uint8_t>& bytes) {
  sockaddr_nl kernel{};
  kernel.nl_family = AF_NETLINK;
  return sendto(fd, bytes.data(), bytes.size(), 0,
                reinterpret_cast<sockaddr*>(&kernel), sizeof(kernel));
}

bool NetAdmin() {
  __user_cap_header_struct header{};
  header.version = _LINUX_CAPABILITY_VERSION_3;
  __user_cap_data_struct data[2]{};
  return syscall(SYS_capget, &header, data) == 0 &&
         (data[0].effective & (uint32_t{1} << CAP_NET_ADMIN));
}

void Ack(int fd, uint32_t seq, int error, uint16_t type) {
  pollfd p{fd, POLLIN, 0};
  ASSERT_EQ(1, poll(&p, 1, 2000));
  ASSERT_TRUE(p.revents & POLLIN);
  std::array<uint8_t, 512> bytes{};
  sockaddr_nl sender{};
  socklen_t size = sizeof(sender);
  const ssize_t n = recvfrom(fd, bytes.data(), bytes.size(), 0,
                             reinterpret_cast<sockaddr*>(&sender), &size);
  ASSERT_GE(n, static_cast<ssize_t>(NLMSG_LENGTH(sizeof(nlmsgerr))));
  EXPECT_EQ(0u, sender.nl_pid);
  nlmsghdr h{};
  nlmsgerr e{};
  std::memcpy(&h, bytes.data(), sizeof(h));
  std::memcpy(&e, bytes.data() + NLMSG_HDRLEN, sizeof(e));
  EXPECT_EQ(NLMSG_ERROR, h.nlmsg_type);
  EXPECT_EQ(seq, h.nlmsg_seq);
  EXPECT_EQ(-error, e.error) << "netlink sequence " << seq;
  EXPECT_EQ(seq, e.msg.nlmsg_seq);
  EXPECT_EQ(type, e.msg.nlmsg_type);
}

struct RuleInfo {
  uint64_t handle = 0;
  uint64_t position = 0;
  uint16_t flags = 0;
  bool has_counter = false;
  uint64_t packets = 0;
  uint64_t bytes = 0;
  std::vector<uint32_t> bitwise_ops;
  bool valid_bitwise_dump = true;
  std::vector<uint32_t> meta_keys;
  bool valid_meta_dump = true;
};

struct NlaSpan {
  const uint8_t* data = nullptr;
  size_t length = 0;
};

NlaSpan FindNla(NlaSpan region, uint16_t kind) {
  for (size_t offset = 0; offset + sizeof(nlattr) <= region.length;) {
    nlattr attr{};
    std::memcpy(&attr, region.data + offset, sizeof(attr));
    if (attr.nla_len < sizeof(attr) || offset + attr.nla_len > region.length) break;
    if ((attr.nla_type & NLA_TYPE_MASK) == kind)
      return {region.data + offset + sizeof(attr), attr.nla_len - sizeof(attr)};
    offset += NLA_ALIGN(attr.nla_len);
  }
  return {};
}

uint64_t Be64(NlaSpan value) {
  if (value.length != 8) return 0;
  uint64_t number = 0;
  for (size_t index = 0; index < 8; ++index)
    number = (number << 8) | value.data[index];
  return number;
}

uint32_t Be32(NlaSpan value) {
  if (value.length != 4) return 0;
  uint32_t number = 0;
  for (size_t index = 0; index < 4; ++index)
    number = (number << 8) | value.data[index];
  return number;
}

void ReadRuleExpressions(const nlmsghdr* message, RuleInfo* rule) {
  const auto* bytes = reinterpret_cast<const uint8_t*>(message);
  const size_t header = NLMSG_HDRLEN + sizeof(nfgenmsg);
  if (message->nlmsg_len < header) return;
  auto expressions = FindNla({bytes + header, message->nlmsg_len - header},
                             NFTA_RULE_EXPRESSIONS);
  for (size_t offset = 0; offset + sizeof(nlattr) <= expressions.length;) {
    nlattr attr{};
    std::memcpy(&attr, expressions.data + offset, sizeof(attr));
    if (attr.nla_len < sizeof(attr) || offset + attr.nla_len > expressions.length) break;
    if ((attr.nla_type & NLA_TYPE_MASK) == NFTA_LIST_ELEM) {
      NlaSpan element{expressions.data + offset + sizeof(attr),
                      attr.nla_len - sizeof(attr)};
      auto name = FindNla(element, NFTA_EXPR_NAME);
      if (name.length == sizeof("counter") &&
          std::memcmp(name.data, "counter", name.length) == 0) {
        auto data = FindNla(element, NFTA_EXPR_DATA);
        auto packets = FindNla(data, NFTA_COUNTER_PACKETS);
        auto counted_bytes = FindNla(data, NFTA_COUNTER_BYTES);
        if (packets.length == 8 && counted_bytes.length == 8) {
          rule->has_counter = true;
          rule->packets = Be64(packets);
          rule->bytes = Be64(counted_bytes);
        }
      } else if (name.length == sizeof("bitwise") &&
                 std::memcmp(name.data, "bitwise", name.length) == 0) {
        auto data = FindNla(element, NFTA_EXPR_DATA);
        auto len = FindNla(data, NFTA_BITWISE_LEN);
        auto op = FindNla(data, NFTA_BITWISE_OP);
        rule->valid_bitwise_dump &= len.length == 4 && Be32(len) == 1 &&
                                    op.length == 4;
        const uint32_t operation = Be32(op);
        rule->bitwise_ops.push_back(operation);
        if (operation == NFT_BITWISE_BOOL) {
          auto mask = FindNla(FindNla(data, NFTA_BITWISE_MASK), NFTA_DATA_VALUE);
          auto xor_value = FindNla(FindNla(data, NFTA_BITWISE_XOR), NFTA_DATA_VALUE);
          rule->valid_bitwise_dump &= mask.length == 1 && mask.data[0] == 0x1f &&
                                      xor_value.length == 1 && xor_value.data[0] == 1;
        } else {
          auto shift = FindNla(FindNla(data, NFTA_BITWISE_DATA), NFTA_DATA_VALUE);
          uint32_t amount = 0;
          if (shift.length == 4) std::memcpy(&amount, shift.data, 4);
          rule->valid_bitwise_dump &= shift.length == 4 && amount == 1;
        }
      } else if (name.length == sizeof("meta") &&
                 std::memcmp(name.data, "meta", name.length) == 0) {
        auto data = FindNla(element, NFTA_EXPR_DATA);
        auto key = FindNla(data, NFTA_META_KEY);
        auto dreg = FindNla(data, NFTA_META_DREG);
        rule->valid_meta_dump &= key.length == 4 && dreg.length == 4 &&
                                 Be32(dreg) == NFT_REG_1;
        rule->meta_keys.push_back(Be32(key));
      }
    }
    offset += NLA_ALIGN(attr.nla_len);
  }
}

uint64_t RuleU64Attr(const nlmsghdr* message, uint16_t type) {
  const auto* bytes = reinterpret_cast<const uint8_t*>(message);
  size_t offset = NLMSG_HDRLEN + sizeof(nfgenmsg);
  while (offset + sizeof(nlattr) <= message->nlmsg_len) {
    nlattr attr{};
    std::memcpy(&attr, bytes + offset, sizeof(attr));
    if (attr.nla_len < sizeof(attr) || offset + attr.nla_len > message->nlmsg_len)
      break;
    if ((attr.nla_type & NLA_TYPE_MASK) == type &&
        attr.nla_len >= sizeof(attr) + sizeof(uint64_t)) {
      uint64_t value = 0;
      for (size_t index = 0; index < sizeof(value); ++index)
        value = (value << 8) | bytes[offset + sizeof(attr) + index];
      return value;
    }
    offset += NLA_ALIGN(attr.nla_len);
  }
  return 0;
}

std::vector<RuleInfo> ReceiveRuleDump(int fd, uint32_t seq) {
  std::vector<RuleInfo> rules;
  for (int attempt = 0; attempt < 16; ++attempt) {
    pollfd ready{fd, POLLIN, 0};
    if (poll(&ready, 1, 2000) != 1) {
      ADD_FAILURE() << "rule dump timed out";
      break;
    }
    std::array<uint8_t, 4096> bytes{};
    const ssize_t count = recv(fd, bytes.data(), bytes.size(), 0);
    if (count < static_cast<ssize_t>(NLMSG_HDRLEN)) {
      ADD_FAILURE() << "short rule dump reply";
      break;
    }
    int remaining = count;
    for (auto* reply = reinterpret_cast<nlmsghdr*>(bytes.data());
         NLMSG_OK(reply, remaining); reply = NLMSG_NEXT(reply, remaining)) {
      if (reply->nlmsg_seq != seq) continue;
      if (reply->nlmsg_type == NLMSG_DONE) return rules;
      if (reply->nlmsg_type != kNftNewrule) {
        ADD_FAILURE() << "unexpected rule dump reply: " << reply->nlmsg_type;
        return rules;
      }
      RuleInfo info{RuleU64Attr(reply, NFTA_RULE_HANDLE),
                    RuleU64Attr(reply, NFTA_RULE_POSITION), reply->nlmsg_flags};
      ReadRuleExpressions(reply, &info);
      rules.push_back(info);
    }
  }
  return rules;
}

size_t ReceiveSetElementDumpCount(int fd, uint32_t seq) {
  size_t total = 0;
  for (int attempt = 0; attempt < 16; ++attempt) {
    pollfd ready{fd, POLLIN, 0};
    if (poll(&ready, 1, 2000) != 1) {
      ADD_FAILURE() << "set element dump timed out";
      return total;
    }
    std::array<uint8_t, 4096> bytes{};
    const ssize_t count = recv(fd, bytes.data(), bytes.size(), 0);
    if (count < static_cast<ssize_t>(NLMSG_HDRLEN)) {
      ADD_FAILURE() << "short set element dump reply";
      return total;
    }
    int remaining = count;
    for (auto* reply = reinterpret_cast<nlmsghdr*>(bytes.data());
         NLMSG_OK(reply, remaining); reply = NLMSG_NEXT(reply, remaining)) {
      if (reply->nlmsg_seq != seq) continue;
      if (reply->nlmsg_type == NLMSG_DONE) return total;
      if (reply->nlmsg_type != kNftNewsetelem) {
        ADD_FAILURE() << "unexpected set element dump reply: " << reply->nlmsg_type;
        return total;
      }
      const auto elements = FindNla(
          {reinterpret_cast<const uint8_t*>(NLMSG_DATA(reply)) + sizeof(nfgenmsg),
           static_cast<size_t>(NLMSG_PAYLOAD(reply, sizeof(nfgenmsg)))},
          NFTA_SET_ELEM_LIST_ELEMENTS);
      for (size_t offset = 0; offset + sizeof(nlattr) <= elements.length;) {
        nlattr item{};
        std::memcpy(&item, elements.data + offset, sizeof(item));
        if (item.nla_len < sizeof(item) || item.nla_len > elements.length - offset) {
          ADD_FAILURE() << "malformed set element dump";
          return total;
        }
        ++total;
        offset += NLA_ALIGN(item.nla_len);
      }
    }
  }
  ADD_FAILURE() << "set element dump did not terminate";
  return total;
}

void Child(const std::function<int()>& fn) {
  const pid_t pid = fork();
  ASSERT_GE(pid, 0);
  if (pid == 0) _exit(fn());
  int status = 0;
  ASSERT_EQ(pid, waitpid(pid, &status, 0));
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(NetlinkNetfilter, SocketTypesFlagsAndEmptyReceive) {
  for (int type : {SOCK_RAW, SOCK_DGRAM}) {
    Fd fd(Open(type));
    ASSERT_GE(fd.get(), 0) << std::strerror(errno);
    EXPECT_NE(0, fcntl(fd.get(), F_GETFL) & O_NONBLOCK);
    EXPECT_NE(0, fcntl(fd.get(), F_GETFD) & FD_CLOEXEC);
    ASSERT_EQ(0, Bind(fd.get()));
    EXPECT_NE(0u, Port(fd.get()));
    char byte;
    EXPECT_EQ(-1, recv(fd.get(), &byte, 1, 0));
    EXPECT_EQ(EAGAIN, errno);
    pollfd p{fd.get(), POLLIN, 0};
    EXPECT_EQ(0, poll(&p, 1, 0));
  }
}

TEST(NetlinkNetfilter, TcpLoopbackSurvivesDeferredTransportOutput) {
  Fd listener(socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0));
  ASSERT_GE(listener.get(), 0);
  sockaddr_in address{};
  address.sin_family = AF_INET;
  address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  ASSERT_EQ(0, bind(listener.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)));
  ASSERT_EQ(0, listen(listener.get(), 1));
  socklen_t address_len = sizeof(address);
  ASSERT_EQ(0, getsockname(listener.get(), reinterpret_cast<sockaddr*>(&address), &address_len));

  Fd client(socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0));
  ASSERT_GE(client.get(), 0);
  const int connected = connect(client.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address));
  ASSERT_TRUE(connected == 0 || (connected == -1 && errno == EINPROGRESS))
      << std::strerror(errno);
  pollfd ready{listener.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 5000));
  Fd server(accept(listener.get(), nullptr, nullptr));
  ASSERT_GE(server.get(), 0) << std::strerror(errno);
  ready = pollfd{client.get(), POLLOUT, 0};
  ASSERT_EQ(1, poll(&ready, 1, 5000));
  int error = -1;
  socklen_t error_len = sizeof(error);
  ASSERT_EQ(0, getsockopt(client.get(), SOL_SOCKET, SO_ERROR, &error, &error_len));
  ASSERT_EQ(0, error);

  constexpr char message[] = "nft-tcp";
  ASSERT_EQ(sizeof(message), static_cast<size_t>(send(client.get(), message, sizeof(message), 0)));
  ready = pollfd{server.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 5000));
  char received[sizeof(message)]{};
  ASSERT_EQ(sizeof(received), static_cast<size_t>(recv(server.get(), received, sizeof(received), MSG_WAITALL)));
  EXPECT_EQ(0, std::memcmp(message, received, sizeof(message)));
}

TEST(NetlinkNetfilter, NftOutputDropDefersTcpSynUntilRulesRemoved) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  ASSERT_GE(nft.get(), 0);
  const std::string table = "dkc035_tcp_out_" + std::to_string(getpid());
  constexpr uint32_t seq = 1490;
  auto batch = TableBatch(kNftNewtable, seq, table.c_str());
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto chain = NewChain(seq + 2, table.c_str(), "output", true, NF_DROP,
                        NF_INET_LOCAL_OUT);
  batch.insert(batch.end(), chain.begin(), chain.end());
  auto end = Request(NFNL_MSG_BATCH_END, seq + 3);
  batch.insert(batch.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewchain);

  Fd listener(socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0));
  ASSERT_GE(listener.get(), 0);
  sockaddr_in address{};
  address.sin_family = AF_INET;
  address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  ASSERT_EQ(0, bind(listener.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)));
  ASSERT_EQ(0, listen(listener.get(), 1));
  socklen_t address_len = sizeof(address);
  ASSERT_EQ(0, getsockname(listener.get(), reinterpret_cast<sockaddr*>(&address), &address_len));
  Fd client(socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0));
  ASSERT_GE(client.get(), 0);
  errno = 0;
  EXPECT_EQ(-1, connect(client.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)));
  EXPECT_EQ(EINPROGRESS, errno);
  pollfd ready{listener.get(), POLLIN, 0};
  EXPECT_EQ(0, poll(&ready, 1, 250));

  auto remove = TableBatch(kNftDeltable, seq + 4, table.c_str());
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
  Ack(nft.get(), seq + 5, 0, kNftDeltable);
  ready.revents = 0;
  ASSERT_EQ(1, poll(&ready, 1, 5000));
  Fd server(accept(listener.get(), nullptr, nullptr));
  ASSERT_GE(server.get(), 0);
  ready = pollfd{client.get(), POLLOUT, 0};
  ASSERT_EQ(1, poll(&ready, 1, 5000));
  int error = -1;
  socklen_t error_len = sizeof(error);
  ASSERT_EQ(0, getsockopt(client.get(), SOL_SOCKET, SO_ERROR, &error, &error_len));
  EXPECT_EQ(0, error);
}

TEST(NetlinkNetfilter, NftOutputDropKeepsNewTcpDataOnPersistTimer) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  Fd listener(socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0));
  ASSERT_GE(nft.get(), 0);
  ASSERT_GE(listener.get(), 0);
  sockaddr_in address{};
  address.sin_family = AF_INET;
  address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  ASSERT_EQ(0, bind(listener.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)));
  ASSERT_EQ(0, listen(listener.get(), 1));
  socklen_t address_len = sizeof(address);
  ASSERT_EQ(0, getsockname(listener.get(), reinterpret_cast<sockaddr*>(&address), &address_len));
  Fd client(socket(AF_INET, SOCK_STREAM, 0));
  ASSERT_GE(client.get(), 0);
  ASSERT_EQ(0, connect(client.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)));
  pollfd listener_ready{listener.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&listener_ready, 1, 5000));
  Fd server(accept(listener.get(), nullptr, nullptr));
  ASSERT_GE(server.get(), 0);

  const std::string table = "dkc035_tcp_persist_" + std::to_string(getpid());
  constexpr uint32_t seq = 1530;
  auto batch = TableBatch(kNftNewtable, seq, table.c_str());
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto chain = NewChain(seq + 2, table.c_str(), "output", true, NF_DROP,
                        NF_INET_LOCAL_OUT);
  batch.insert(batch.end(), chain.begin(), chain.end());
  auto end = Request(NFNL_MSG_BATCH_END, seq + 3);
  batch.insert(batch.end(), end.begin(), end.end());
  bool installed = false;
  [&] {
    ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
    installed = true;
    Ack(nft.get(), seq + 1, 0, kNftNewtable);
    Ack(nft.get(), seq + 2, 0, kNftNewchain);
    constexpr char data[] = "persist";
    ASSERT_EQ(sizeof(data), static_cast<size_t>(send(client.get(), data, sizeof(data), 0)));
    pollfd ready{server.get(), POLLIN, 0};
    EXPECT_EQ(0, poll(&ready, 1, 50));
  }();
  if (installed) {
    auto remove = TableBatch(kNftDeltable, seq + 4, table.c_str());
    ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
    Ack(nft.get(), seq + 5, 0, kNftDeltable);
    pollfd ready{server.get(), POLLIN, 0};
    // Deleting a rule does not itself wake Linux's unsent-data probe timer.
    EXPECT_EQ(0, poll(&ready, 1, 50));
    ASSERT_EQ(1, poll(&ready, 1, 2000));
    char received[sizeof("persist")]{};
    ASSERT_EQ(sizeof(received), static_cast<size_t>(recv(server.get(), received,
                                                         sizeof(received), MSG_WAITALL)));
    EXPECT_EQ(0, std::memcmp(received, "persist", sizeof(received)));
  }
}

TEST(NetlinkNetfilter, NftOutputFiltersKernelGeneratedIcmp) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  Fd observer(socket(AF_INET, SOCK_RAW | SOCK_NONBLOCK, IPPROTO_ICMP));
  Fd sender(socket(AF_INET, SOCK_DGRAM, 0));
  ASSERT_GE(nft.get(), 0);
  ASSERT_GE(observer.get(), 0);
  ASSERT_GE(sender.get(), 0);

  sockaddr_in destination{};
  destination.sin_family = AF_INET;
  destination.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  socklen_t destination_len = sizeof(destination);
  {
    Fd temporary(socket(AF_INET, SOCK_DGRAM, 0));
    ASSERT_GE(temporary.get(), 0);
    ASSERT_EQ(0, bind(temporary.get(), reinterpret_cast<sockaddr*>(&destination),
                      sizeof(destination)));
    ASSERT_EQ(0, getsockname(temporary.get(), reinterpret_cast<sockaddr*>(&destination),
                             &destination_len));
  }
  // Releasing the reserved port makes the UDP probe generate Port Unreachable.
  const uint8_t probe = 0x35;
  auto send_probe = [&] {
    return sendto(sender.get(), &probe, 1, 0,
                  reinterpret_cast<sockaddr*>(&destination), destination_len);
  };
  auto receive_icmp = [&](int timeout_ms) {
    pollfd ready{observer.get(), POLLIN, 0};
    if (poll(&ready, 1, timeout_ms) != 1) return false;
    std::array<uint8_t, 256> packet{};
    const ssize_t size = recv(observer.get(), packet.data(), packet.size(), 0);
    if (size < 22 || packet[0] >> 4 != 4) return false;
    const size_t header_len = (packet[0] & 0x0f) * 4;
    return header_len + 2 <= static_cast<size_t>(size) &&
           packet[header_len] == 3 && packet[header_len + 1] == 3;
  };
  ASSERT_EQ(1, send_probe());
  ASSERT_TRUE(receive_icmp(2000));

  const std::string table = "dkc035_icmp_out_" + std::to_string(getpid());
  constexpr uint32_t seq = 1520;
  auto batch = TableBatch(kNftNewtable, seq, table.c_str());
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto chain = NewChain(seq + 2, table.c_str(), "output", true, NF_ACCEPT,
                        NF_INET_LOCAL_OUT);
  auto rule = ProtocolCounterRule(seq + 3, table.c_str(), "output", NFT_CMP_EQ,
                                  NFT_PAYLOAD_NETWORK_HEADER, NFT_REG_1, 9,
                                  UINT32_MAX, false, 1, true, nullptr, IPPROTO_ICMP);
  batch.insert(batch.end(), chain.begin(), chain.end());
  batch.insert(batch.end(), rule.begin(), rule.end());
  auto end = Request(NFNL_MSG_BATCH_END, seq + 4);
  batch.insert(batch.end(), end.begin(), end.end());
  bool installed = false;
  [&] {
    ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
    installed = true;
    Ack(nft.get(), seq + 1, 0, kNftNewtable);
    Ack(nft.get(), seq + 2, 0, kNftNewchain);
    Ack(nft.get(), seq + 3, 0, kNftNewrule);
    ASSERT_EQ(1, send_probe());
    pollfd ready{observer.get(), POLLIN, 0};
    EXPECT_EQ(0, poll(&ready, 1, 250));
  }();
  if (installed) {
    auto remove = TableBatch(kNftDeltable, seq + 5, table.c_str());
    ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
    Ack(nft.get(), seq + 6, 0, kNftDeltable);
    ASSERT_EQ(1, send_probe());
    EXPECT_TRUE(receive_icmp(2000));
  }
}

TEST(NetlinkNetfilter, IndependentPortsAndCloseReleasesBinding) {
  uint32_t port;
  Fd duplicate(Open());
  ASSERT_GE(duplicate.get(), 0);
  {
    Fd first(Open());
    Fd automatic(Open());
    Fd route(Open(SOCK_RAW, NETLINK_ROUTE));
    ASSERT_GE(first.get(), 0);
    ASSERT_GE(automatic.get(), 0);
    ASSERT_GE(route.get(), 0);
    ASSERT_EQ(0, Bind(first.get()));
    port = Port(first.get());
    ASSERT_NE(0u, port);
    ASSERT_EQ(0, Bind(automatic.get()));
    EXPECT_NE(port, Port(automatic.get()));
    EXPECT_EQ(-1, Bind(duplicate.get(), port));
    EXPECT_EQ(EADDRINUSE, errno);
    EXPECT_EQ(0, Bind(route.get(), port));
  }
  EXPECT_EQ(0, Bind(duplicate.get(), port));
}

TEST(NetlinkNetfilter, NamespacePortIsolation) {
  Fd parent(Open());
  ASSERT_GE(parent.get(), 0);
  ASSERT_EQ(0, Bind(parent.get()));
  const uint32_t port = Port(parent.get());
  ASSERT_NE(0u, port);
  Child([port]() {
    if (unshare(CLONE_NEWNET) < 0) return 1;
    Fd child(Open());
    if (child.get() < 0 || Bind(child.get(), port) < 0) return 2;
    return Port(child.get()) == port ? 0 : 3;
  });
}

TEST(NetlinkNetfilter, UnknownSubsystemAndMultipleMessages) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  auto first = Request(kUnknown, 41);
  auto second = Request(kUnknown | 1, 42);
  first.insert(first.end(), second.begin(), second.end());
  ASSERT_EQ(static_cast<ssize_t>(first.size()), Send(fd.get(), first));
  Ack(fd.get(), 41, EINVAL, kUnknown);
  Ack(fd.get(), 42, EINVAL, kUnknown | 1);
}

TEST(NetlinkNetfilter, NftGenerationReplyAndOptionalAck) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  ASSERT_EQ(0, Bind(fd.get()));
  const uint32_t port = Port(fd.get());
  ASSERT_NE(0u, port);

  for (bool ack : {false, true}) {
    const uint32_t seq = ack ? 102 : 101;
    auto request = Request(kNftGetgen, seq);
    nlmsghdr header{};
    std::memcpy(&header, request.data(), sizeof(header));
    if (!ack) header.nlmsg_flags &= ~NLM_F_ACK;
    std::memcpy(request.data(), &header, sizeof(header));
    ASSERT_EQ(static_cast<ssize_t>(request.size()), Send(fd.get(), request));

    pollfd p{fd.get(), POLLIN, 0};
    ASSERT_EQ(1, poll(&p, 1, 2000));
    std::array<uint8_t, 512> bytes{};
    sockaddr_nl sender{};
    socklen_t sender_len = sizeof(sender);
    const ssize_t n = recvfrom(fd.get(), bytes.data(), bytes.size(), 0,
                               reinterpret_cast<sockaddr*>(&sender), &sender_len);
    ASSERT_GE(n, static_cast<ssize_t>(NLMSG_LENGTH(sizeof(nfgenmsg))));
    EXPECT_EQ(0u, sender.nl_pid);
    nlmsghdr reply{};
    std::memcpy(&reply, bytes.data(), sizeof(reply));
    ASSERT_EQ(n, static_cast<ssize_t>(reply.nlmsg_len));
    EXPECT_EQ(kNftNewgen, reply.nlmsg_type);
    EXPECT_EQ(seq, reply.nlmsg_seq);
    EXPECT_EQ(port, reply.nlmsg_pid);
    nfgenmsg gen{};
    std::memcpy(&gen, bytes.data() + NLMSG_HDRLEN, sizeof(gen));
    EXPECT_EQ(AF_UNSPEC, gen.nfgen_family);
    EXPECT_EQ(NFNETLINK_V0, gen.version);

    bool found_id = false;
    bool found_pid = false;
    bool found_name = false;
    size_t offset = NLMSG_HDRLEN + sizeof(gen);
    while (offset + sizeof(nlattr) <= static_cast<size_t>(n)) {
      nlattr attr{};
      std::memcpy(&attr, bytes.data() + offset, sizeof(attr));
      ASSERT_GE(attr.nla_len, sizeof(nlattr));
      ASSERT_LE(offset + attr.nla_len, static_cast<size_t>(n));
      const uint8_t* value = bytes.data() + offset + sizeof(attr);
      if (attr.nla_type == NFTA_GEN_ID) {
        ASSERT_EQ(sizeof(attr) + sizeof(uint32_t), attr.nla_len);
        uint32_t generation = 0;
        std::memcpy(&generation, value, sizeof(generation));
        generation = ntohl(generation);
        EXPECT_NE(0u, generation);
        EXPECT_EQ(static_cast<uint16_t>(generation), ntohs(gen.res_id));
        found_id = true;
      } else if (attr.nla_type == NFTA_GEN_PROC_PID) {
        ASSERT_EQ(sizeof(attr) + sizeof(uint32_t), attr.nla_len);
        uint32_t pid = 0;
        std::memcpy(&pid, value, sizeof(pid));
        EXPECT_EQ(static_cast<uint32_t>(syscall(SYS_gettid)), ntohl(pid));
        found_pid = true;
      } else if (attr.nla_type == NFTA_GEN_PROC_NAME) {
        ASSERT_GT(attr.nla_len, sizeof(attr));
        EXPECT_EQ(0, value[attr.nla_len - sizeof(attr) - 1]);
        EXPECT_LE(attr.nla_len - sizeof(attr), 16u);
        found_name = true;
      }
      offset += NLA_ALIGN(attr.nla_len);
    }
    EXPECT_TRUE(found_id);
    EXPECT_TRUE(found_pid);
    EXPECT_TRUE(found_name);
    if (ack) Ack(fd.get(), seq, 0, kNftGetgen);
    EXPECT_EQ(0, poll(&p, 1, 0));
  }
}

TEST(NetlinkNetfilter, NftEmptyTableDumpEndsWithoutExtraAck) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  ASSERT_EQ(0, Bind(fd.get()));
  auto request = Request(kNftGettable, 103);
  nlmsghdr header{};
  std::memcpy(&header, request.data(), sizeof(header));
  header.nlmsg_flags |= NLM_F_DUMP;
  std::memcpy(request.data(), &header, sizeof(header));
  ASSERT_EQ(static_cast<ssize_t>(request.size()), Send(fd.get(), request));

  pollfd p{fd.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&p, 1, 2000));
  std::array<uint8_t, 512> bytes{};
  sockaddr_nl sender{};
  socklen_t sender_len = sizeof(sender);
  const ssize_t n = recvfrom(fd.get(), bytes.data(), bytes.size(), 0,
                             reinterpret_cast<sockaddr*>(&sender), &sender_len);
  ASSERT_GE(n, static_cast<ssize_t>(NLMSG_LENGTH(sizeof(int))));
  EXPECT_EQ(0u, sender.nl_pid);
  nlmsghdr reply{};
  std::memcpy(&reply, bytes.data(), sizeof(reply));
  EXPECT_EQ(n, static_cast<ssize_t>(reply.nlmsg_len));
  EXPECT_EQ(NLMSG_DONE, reply.nlmsg_type);
  EXPECT_EQ(103u, reply.nlmsg_seq);
  EXPECT_EQ(0, poll(&p, 1, 0));
}

TEST(NetlinkNetfilter, ShortBatchBeginCannotReadNextMessageAsNfgen) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  auto malformed = Request(NFNL_MSG_BATCH_BEGIN, 108);
  nlmsghdr header{};
  std::memcpy(&header, malformed.data(), sizeof(header));
  header.nlmsg_len = NLMSG_HDRLEN;
  std::memcpy(malformed.data(), &header, sizeof(header));
  // These bytes are in the datagram but outside the first nlmsg. They used
  // to fool the subsystem check before a 20-byte slice of a 16-byte nlmsg.
  malformed[18] = 0;
  malformed[19] = NFNL_SUBSYS_NFTABLES;
  ASSERT_EQ(static_cast<ssize_t>(malformed.size()), Send(fd.get(), malformed));

  auto valid = Request(kNftGetgen, 109);
  ASSERT_EQ(static_cast<ssize_t>(valid.size()), Send(fd.get(), valid));
  pollfd ready{fd.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 2000));
  std::array<uint8_t, 512> bytes{};
  const ssize_t count = recv(fd.get(), bytes.data(), bytes.size(), 0);
  ASSERT_GE(count, static_cast<ssize_t>(NLMSG_HDRLEN));
  std::memcpy(&header, bytes.data(), sizeof(header));
  EXPECT_EQ(kNftNewgen, header.nlmsg_type);
  EXPECT_EQ(109u, header.nlmsg_seq);
  Ack(fd.get(), 109, 0, kNftGetgen);
}

TEST(NetlinkNetfilter, MalformedBatchTailDoesNotAckRolledBackTable) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  constexpr uint32_t seq = 113;
  auto batch = TableBatch(kNftNewtable, seq, "dkc035_abort");
  nlmsghdr malformed{};
  std::memcpy(&malformed, batch.data() + batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)),
              sizeof(malformed));
  malformed.nlmsg_len = 8;
  std::memcpy(batch.data() + batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)),
              &malformed, sizeof(malformed));
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(fd.get(), batch));

  auto query = Request(kNftGettable, seq + 3);
  query[NLMSG_HDRLEN] = NFPROTO_IPV4;
  AppendAttr(&query, NFTA_TABLE_NAME, "dkc035_abort");
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(fd.get(), query));
  bool saw_lookup_error = false;
  for (int attempt = 0; attempt < 4 && !saw_lookup_error; ++attempt) {
    pollfd ready{fd.get(), POLLIN, 0};
    ASSERT_EQ(1, poll(&ready, 1, 2000));
    std::array<uint8_t, 512> bytes{};
    const ssize_t count = recv(fd.get(), bytes.data(), bytes.size(), 0);
    ASSERT_GE(count, static_cast<ssize_t>(NLMSG_LENGTH(sizeof(nlmsgerr))));
    nlmsghdr reply{};
    nlmsgerr error{};
    std::memcpy(&reply, bytes.data(), sizeof(reply));
    std::memcpy(&error, bytes.data() + NLMSG_HDRLEN, sizeof(error));
    if (reply.nlmsg_seq == seq + 1 && reply.nlmsg_type == NLMSG_ERROR) {
      EXPECT_NE(0, error.error) << "rolled-back NEWTABLE must not succeed";
    }
    if (reply.nlmsg_seq == seq + 3 && reply.nlmsg_type == NLMSG_ERROR) {
      EXPECT_EQ(-ENOENT, error.error);
      saw_lookup_error = true;
    }
  }
  EXPECT_TRUE(saw_lookup_error);
}

TEST(NetlinkNetfilter, MissingBatchEndRetainsEarlierRequestAck) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  constexpr uint32_t seq = 117;
  auto batch = TableBatch(kNftNewtable, seq, "dkc035_no_end");
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(fd.get(), batch));
  // Linux aborts this incomplete transaction, but still delivers the ACK
  // queued for the individually successful NEWTABLE request.
  Ack(fd.get(), seq + 1, 0, kNftNewtable);

  auto query = Request(kNftGettable, seq + 3);
  query[NLMSG_HDRLEN] = NFPROTO_IPV4;
  AppendAttr(&query, NFTA_TABLE_NAME, "dkc035_no_end");
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(fd.get(), query));
  Ack(fd.get(), seq + 3, ENOENT, kNftGettable);
}

TEST(NetlinkNetfilter, NonRequestInBatchKeepsEarlierAckButAborts) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  constexpr uint32_t seq = 121;
  auto batch = TableBatch(kNftNewtable, seq, "dkc035_no_request");
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto invalid = Request(kNftGetgen, seq + 2);
  nlmsghdr header{};
  std::memcpy(&header, invalid.data(), sizeof(header));
  header.nlmsg_flags &= ~NLM_F_REQUEST;
  std::memcpy(invalid.data(), &header, sizeof(header));
  auto end = Request(NFNL_MSG_BATCH_END, seq + 3);
  batch.insert(batch.end(), invalid.begin(), invalid.end());
  batch.insert(batch.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(fd.get(), batch));
  Ack(fd.get(), seq + 1, 0, kNftNewtable);
  Ack(fd.get(), seq + 2, EINVAL, kNftGetgen);

  auto query = Request(kNftGettable, seq + 4);
  query[NLMSG_HDRLEN] = NFPROTO_IPV4;
  AppendAttr(&query, NFTA_TABLE_NAME, "dkc035_no_request");
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(fd.get(), query));
  Ack(fd.get(), seq + 4, ENOENT, kNftGettable);
}

TEST(NetlinkNetfilter, NftEmptyObjectDumpsEndWithoutFalseSuccessAck) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  ASSERT_EQ(0, Bind(fd.get()));
  uint32_t seq = 130;
  for (const uint16_t kind : {kNftGetchain, kNftGetset, kNftGetflowtable}) {
    auto request = Request(kind, seq);
    nlmsghdr header{};
    std::memcpy(&header, request.data(), sizeof(header));
    header.nlmsg_flags |= NLM_F_DUMP;
    std::memcpy(request.data(), &header, sizeof(header));
    ASSERT_EQ(static_cast<ssize_t>(request.size()), Send(fd.get(), request));

    pollfd ready{fd.get(), POLLIN, 0};
    ASSERT_EQ(1, poll(&ready, 1, 2000));
    std::array<uint8_t, 512> bytes{};
    const ssize_t count = recv(fd.get(), bytes.data(), bytes.size(), 0);
    ASSERT_GE(count, static_cast<ssize_t>(NLMSG_LENGTH(sizeof(int))));
    nlmsghdr reply{};
    std::memcpy(&reply, bytes.data(), sizeof(reply));
    EXPECT_EQ(NLMSG_DONE, reply.nlmsg_type);
    EXPECT_EQ(seq, reply.nlmsg_seq);
    EXPECT_EQ(count, static_cast<ssize_t>(reply.nlmsg_len));
    EXPECT_EQ(0, poll(&ready, 1, 0));
    ++seq;
  }
}

TEST(NetlinkNetfilter, NftAbsentObjectsReturnEnoentNotEmptyDump) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  uint32_t seq = 140;
  for (const auto [kind, table_attr, name_attr] : {
           std::array<uint16_t, 3>{kNftGetchain, NFTA_CHAIN_TABLE, NFTA_CHAIN_NAME},
           std::array<uint16_t, 3>{kNftGetset, NFTA_SET_TABLE, NFTA_SET_NAME},
           std::array<uint16_t, 3>{kNftGetflowtable, NFTA_FLOWTABLE_TABLE,
                                   NFTA_FLOWTABLE_NAME},
       }) {
    auto query = Request(kind, seq);
    query[NLMSG_HDRLEN] = NFPROTO_IPV4;
    AppendAttr(&query, table_attr, "dkc035_missing_table");
    AppendAttr(&query, name_attr, "dkc035_missing_object");
    ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(fd.get(), query));
    Ack(fd.get(), seq, ENOENT, kind);
    ++seq;
  }
}

TEST(NetlinkNetfilter, NftReadErrorsFollowLookupAndPolicyOrder) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  uint32_t seq = 150;
  for (const auto& [kind, table_attr, dump] : {
           std::tuple<uint16_t, uint16_t, bool>{kNftGetchain, NFTA_CHAIN_TABLE, false},
           std::tuple<uint16_t, uint16_t, bool>{kNftGetset, NFTA_SET_TABLE, false},
           std::tuple<uint16_t, uint16_t, bool>{kNftGetset, NFTA_SET_TABLE, true},
       }) {
    auto query = Request(kind, seq);
    query[NLMSG_HDRLEN] = NFPROTO_IPV4;
    AppendAttr(&query, table_attr, "dkc035_missing_table");
    if (dump) {
      nlmsghdr header{};
      std::memcpy(&header, query.data(), sizeof(header));
      header.nlmsg_flags |= NLM_F_DUMP;
      std::memcpy(query.data(), &header, sizeof(header));
    }
    ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(fd.get(), query));
    Ack(fd.get(), seq++, ENOENT, kind);
  }

  auto unspec = Request(kNftGetset, seq);
  ASSERT_EQ(static_cast<ssize_t>(unspec.size()), Send(fd.get(), unspec));
  Ack(fd.get(), seq++, EAFNOSUPPORT, kNftGetset);

  for (const auto [kind, short_attr] : {
           std::array<uint16_t, 2>{kNftGetchain, NFTA_CHAIN_HANDLE},
           std::array<uint16_t, 2>{kNftGetset, NFTA_SET_FLAGS},
           std::array<uint16_t, 2>{kNftGetflowtable, NFTA_FLOWTABLE_FLAGS},
       }) {
    auto query = Request(kind, seq, sizeof(nfgenmsg) + NLA_ALIGN(sizeof(nlattr) + 1));
    nlmsghdr header{};
    std::memcpy(&header, query.data(), sizeof(header));
    header.nlmsg_flags |= NLM_F_DUMP;
    std::memcpy(query.data(), &header, sizeof(header));
    nlattr attr{};
    attr.nla_type = short_attr;
    attr.nla_len = sizeof(nlattr) + 1;
    std::memcpy(query.data() + NLMSG_HDRLEN + sizeof(nfgenmsg), &attr, sizeof(attr));
    ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(fd.get(), query));
    Ack(fd.get(), seq++, ERANGE, kind);
  }
}

TEST(NetlinkNetfilter, NftAbsentTableIsNotAnEmptyDump) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  auto request = Request(kNftGettable, 104, sizeof(nfgenmsg) + NLA_ALIGN(12));
  request[NLMSG_HDRLEN] = NFPROTO_IPV4;
  nlattr name{};
  name.nla_type = NFTA_TABLE_NAME;
  name.nla_len = 12;
  std::memcpy(request.data() + NLMSG_HDRLEN + sizeof(nfgenmsg), &name, sizeof(name));
  std::memcpy(request.data() + NLMSG_HDRLEN + sizeof(nfgenmsg) + sizeof(name),
              "missing", 8);
  ASSERT_EQ(static_cast<ssize_t>(request.size()), Send(fd.get(), request));
  Ack(fd.get(), 104, ENOENT, kNftGettable);
}

TEST(NetlinkNetfilter, NftTableBatchCreateQueryAndDelete) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  ASSERT_EQ(0, Bind(fd.get()));
  const char* name = "dkc035_t1";
  auto create = TableBatch(kNftNewtable, 200, name);
  ASSERT_EQ(static_cast<ssize_t>(create.size()), Send(fd.get(), create));
  Ack(fd.get(), 201, 0, kNftNewtable);

  auto query = Request(kNftGettable, 203);
  query[NLMSG_HDRLEN] = NFPROTO_IPV4;
  AppendAttr(&query, NFTA_TABLE_NAME, name);
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(fd.get(), query));
  std::array<uint8_t, 512> bytes{};
  const ssize_t n = recv(fd.get(), bytes.data(), bytes.size(), 0);
  ASSERT_GE(n, static_cast<ssize_t>(NLMSG_HDRLEN + sizeof(nfgenmsg)));
  nlmsghdr reply{};
  std::memcpy(&reply, bytes.data(), sizeof(reply));
  EXPECT_EQ(kNftNewtable, reply.nlmsg_type);
  EXPECT_EQ(203u, reply.nlmsg_seq);
  Ack(fd.get(), 203, 0, kNftGettable);

  auto remove = TableBatch(kNftDeltable, 205, name);
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(fd.get(), remove));
  Ack(fd.get(), 206, 0, kNftDeltable);
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(fd.get(), query));
  Ack(fd.get(), 203, ENOENT, kNftGettable);
}

TEST(NetlinkNetfilter, NftRegularChainHasNoPacketHook) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  constexpr uint32_t seq = 860;
  auto batch = TableBatch(kNftNewtable, seq, "dkc035_regular");
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto chain = Request(kNftNewchain, seq + 2);
  chain[NLMSG_HDRLEN] = NFPROTO_IPV4;
  nlmsghdr header{};
  std::memcpy(&header, chain.data(), sizeof(header));
  header.nlmsg_flags |= NLM_F_CREATE | NLM_F_EXCL;
  std::memcpy(chain.data(), &header, sizeof(header));
  AppendAttr(&chain, NFTA_CHAIN_TABLE, "dkc035_regular");
  AppendAttr(&chain, NFTA_CHAIN_NAME, "user_chain");
  AppendAttr(&chain, NFTA_CHAIN_TYPE, "filter");
  auto end = Request(NFNL_MSG_BATCH_END, seq + 3);
  batch.insert(batch.end(), chain.begin(), chain.end());
  batch.insert(batch.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(fd.get(), batch));
  Ack(fd.get(), seq + 1, 0, kNftNewtable);
  Ack(fd.get(), seq + 2, 0, kNftNewchain);

  auto query = Request(kNftGetchain, seq + 4);
  query[NLMSG_HDRLEN] = NFPROTO_IPV4;
  AppendAttr(&query, NFTA_CHAIN_TABLE, "dkc035_regular");
  AppendAttr(&query, NFTA_CHAIN_NAME, "user_chain");
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(fd.get(), query));
  std::array<uint8_t, 512> bytes{};
  const ssize_t count = recv(fd.get(), bytes.data(), bytes.size(), 0);
  ASSERT_GE(count, static_cast<ssize_t>(NLMSG_HDRLEN + sizeof(nfgenmsg)));
  std::memcpy(&header, bytes.data(), sizeof(header));
  ASSERT_EQ(kNftNewchain, header.nlmsg_type);
  ASSERT_EQ(seq + 4, header.nlmsg_seq);
  bool saw_hook = false;
  for (size_t offset = NLMSG_HDRLEN + sizeof(nfgenmsg);
       offset + sizeof(nlattr) <= header.nlmsg_len;) {
    nlattr attr{};
    std::memcpy(&attr, bytes.data() + offset, sizeof(attr));
    ASSERT_GE(attr.nla_len, sizeof(attr));
    ASSERT_LE(offset + attr.nla_len, header.nlmsg_len);
    saw_hook |= (attr.nla_type & NLA_TYPE_MASK) == NFTA_CHAIN_HOOK;
    offset += NLA_ALIGN(attr.nla_len);
  }
  EXPECT_FALSE(saw_hook);
  Ack(fd.get(), seq + 4, 0, kNftGetchain);

  auto remove = TableBatch(kNftDeltable, seq + 5, "dkc035_regular");
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(fd.get(), remove));
  Ack(fd.get(), seq + 6, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, NftJumpToRegularChainExecutesAndProtectsTarget) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd receiver(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd sender(socket(AF_INET, SOCK_DGRAM, 0));
  Fd nft(Open());
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(sender.get(), 0);
  ASSERT_GE(nft.get(), 0);
  sockaddr_in address{};
  address.sin_family = AF_INET;
  address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  ASSERT_EQ(0, bind(receiver.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)));
  socklen_t address_len = sizeof(address);
  ASSERT_EQ(0, getsockname(receiver.get(), reinterpret_cast<sockaddr*>(&address), &address_len));

  constexpr uint32_t seq = 870;
  auto batch = TableBatch(kNftNewtable, seq, "dkc035_jump");
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto base = NewChain(seq + 2, "dkc035_jump", "prerouting", true, NF_DROP);
  auto regular = NewChain(seq + 3, "dkc035_jump", "user");
  auto jump = ImmediateRule(seq + 4, "dkc035_jump", "prerouting",
                            static_cast<uint32_t>(NFT_JUMP), true, "user");
  auto accept = ImmediateRule(seq + 5, "dkc035_jump", "user", NF_ACCEPT, true);
  auto end = Request(NFNL_MSG_BATCH_END, seq + 6);
  batch.insert(batch.end(), base.begin(), base.end());
  batch.insert(batch.end(), regular.begin(), regular.end());
  batch.insert(batch.end(), jump.begin(), jump.end());
  batch.insert(batch.end(), accept.begin(), accept.end());
  batch.insert(batch.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
  for (uint32_t i = 1; i <= 5; ++i) {
    Ack(nft.get(), seq + i, 0, i == 1 ? kNftNewtable : i <= 3 ? kNftNewchain : kNftNewrule);
  }
  const uint8_t probe = 9;
  ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                      reinterpret_cast<sockaddr*>(&address), address_len));
  pollfd ready{receiver.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 2000));
  uint8_t received = 0;
  ASSERT_EQ(1, recv(receiver.get(), &received, 1, 0));
  EXPECT_EQ(probe, received);

  // Removing a jump target before the referring rule must fail atomically.
  auto remove_chain = Request(kNftDelchain, seq + 8);
  remove_chain[NLMSG_HDRLEN] = NFPROTO_IPV4;
  AppendAttr(&remove_chain, NFTA_CHAIN_TABLE, "dkc035_jump");
  AppendAttr(&remove_chain, NFTA_CHAIN_NAME, "user");
  auto failed_batch = Request(NFNL_MSG_BATCH_BEGIN, seq + 7);
  nfgenmsg begin_gen{};
  begin_gen.res_id = htons(NFNL_SUBSYS_NFTABLES);
  std::memcpy(failed_batch.data() + NLMSG_HDRLEN, &begin_gen, sizeof(begin_gen));
  auto failed_end = Request(NFNL_MSG_BATCH_END, seq + 9);
  failed_batch.insert(failed_batch.end(), remove_chain.begin(), remove_chain.end());
  failed_batch.insert(failed_batch.end(), failed_end.begin(), failed_end.end());
  ASSERT_EQ(static_cast<ssize_t>(failed_batch.size()), Send(nft.get(), failed_batch));
  Ack(nft.get(), seq + 8, EBUSY, kNftDelchain);

  auto remove_table = TableBatch(kNftDeltable, seq + 10, "dkc035_jump");
  ASSERT_EQ(static_cast<ssize_t>(remove_table.size()), Send(nft.get(), remove_table));
  Ack(nft.get(), seq + 11, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, NftSixteenJumpEdgesAbortBeforePublish) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  ASSERT_GE(nft.get(), 0);
  constexpr uint32_t seq = 940;
  constexpr char table_name[] = "dkc035_depth";
  auto batch = JumpDepthBatch(seq, table_name, 16);
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
  for (uint32_t index = 1; index <= 33; ++index) {
    const uint16_t kind = index == 1 ? kNftNewtable : index <= 18 ? kNftNewchain : kNftNewrule;
    Ack(nft.get(), seq + index, 0, kind);
  }
  Ack(nft.get(), seq + 34, EMLINK, kNftNewrule);
  auto query = Request(kNftGettable, seq + 36);
  query[NLMSG_HDRLEN] = NFPROTO_IPV4;
  AppendAttr(&query, NFTA_TABLE_NAME, table_name);
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
  Ack(nft.get(), seq + 36, ENOENT, kNftGettable);
}

TEST(NetlinkNetfilter, NftFifteenJumpEdgesAreAllowed) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  ASSERT_GE(nft.get(), 0);
  constexpr uint32_t seq = 980;
  constexpr char table_name[] = "dkc035_depth_ok";
  auto batch = JumpDepthBatch(seq, table_name, 15);
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
  for (uint32_t index = 1; index <= 32; ++index) {
    const uint16_t kind = index == 1 ? kNftNewtable : index <= 17 ? kNftNewchain : kNftNewrule;
    Ack(nft.get(), seq + index, 0, kind);
  }
  auto remove = TableBatch(kNftDeltable, seq + 34, table_name);
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
  Ack(nft.get(), seq + 35, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, NftJumpReturnsToCallerButGotoUsesBasePolicy) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd receiver(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd sender(socket(AF_INET, SOCK_DGRAM, 0));
  Fd nft(Open());
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(sender.get(), 0);
  ASSERT_GE(nft.get(), 0);
  sockaddr_in address{};
  address.sin_family = AF_INET;
  address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  ASSERT_EQ(0, bind(receiver.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)));
  socklen_t address_len = sizeof(address);
  ASSERT_EQ(0, getsockname(receiver.get(), reinterpret_cast<sockaddr*>(&address), &address_len));

  for (bool use_goto : {false, true}) {
    const char* table_name = use_goto ? "dkc035_goto" : "dkc035_return";
    const uint32_t seq = use_goto ? 1020 : 1000;
    auto batch = TableBatch(kNftNewtable, seq, table_name);
    batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
    auto base = NewChain(seq + 2, table_name, "prerouting", true, NF_DROP);
    auto regular = NewChain(seq + 3, table_name, "user");
    auto call = ImmediateRule(seq + 4, table_name, "prerouting",
                              static_cast<uint32_t>(use_goto ? NFT_GOTO : NFT_JUMP),
                              true, "user");
    auto accept = ImmediateRule(seq + 5, table_name, "prerouting", NF_ACCEPT, true);
    auto ret = ImmediateRule(seq + 6, table_name, "user",
                             static_cast<uint32_t>(NFT_RETURN), true);
    auto end = Request(NFNL_MSG_BATCH_END, seq + 7);
    for (const auto* message : {&base, &regular, &call, &accept, &ret, &end}) {
      batch.insert(batch.end(), message->begin(), message->end());
    }
    ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
    Ack(nft.get(), seq + 1, 0, kNftNewtable);
    Ack(nft.get(), seq + 2, 0, kNftNewchain);
    Ack(nft.get(), seq + 3, 0, kNftNewchain);
    for (uint32_t index = 4; index <= 6; ++index) Ack(nft.get(), seq + index, 0, kNftNewrule);

    const uint8_t probe = use_goto ? 11 : 10;
    ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                        reinterpret_cast<sockaddr*>(&address), address_len));
    pollfd ready{receiver.get(), POLLIN, 0};
    EXPECT_EQ(use_goto ? 0 : 1, poll(&ready, 1, use_goto ? 150 : 2000));
    if (!use_goto) {
      uint8_t received = 0;
      ASSERT_EQ(1, recv(receiver.get(), &received, 1, 0));
      EXPECT_EQ(probe, received);
    }
    auto remove = TableBatch(kNftDeltable, seq + 8, table_name);
    ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
    Ack(nft.get(), seq + 9, 0, kNftDeltable);
  }
}

TEST(NetlinkNetfilter, NftReachableChainCycleRejectsLastRuleAndRollsBack) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  ASSERT_GE(nft.get(), 0);
  constexpr uint32_t seq = 1040;
  constexpr char table_name[] = "dkc035_cycle";
  auto batch = TableBatch(kNftNewtable, seq, table_name);
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto base = NewChain(seq + 2, table_name, "prerouting", true);
  auto a = NewChain(seq + 3, table_name, "a");
  auto b = NewChain(seq + 4, table_name, "b");
  auto to_a = ImmediateRule(seq + 5, table_name, "prerouting",
                            static_cast<uint32_t>(NFT_JUMP), true, "a");
  auto to_b = ImmediateRule(seq + 6, table_name, "a",
                            static_cast<uint32_t>(NFT_GOTO), true, "b");
  auto back = ImmediateRule(seq + 7, table_name, "b",
                            static_cast<uint32_t>(NFT_JUMP), true, "a");
  auto end = Request(NFNL_MSG_BATCH_END, seq + 8);
  for (const auto* message : {&base, &a, &b, &to_a, &to_b, &back, &end}) {
    batch.insert(batch.end(), message->begin(), message->end());
  }
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  for (uint32_t index = 2; index <= 4; ++index) Ack(nft.get(), seq + index, 0, kNftNewchain);
  for (uint32_t index = 5; index <= 6; ++index) Ack(nft.get(), seq + index, 0, kNftNewrule);
  Ack(nft.get(), seq + 7, EMLINK, kNftNewrule);
  auto query = Request(kNftGettable, seq + 9);
  query[NLMSG_HDRLEN] = NFPROTO_IPV4;
  AppendAttr(&query, NFTA_TABLE_NAME, table_name);
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
  Ack(nft.get(), seq + 9, ENOENT, kNftGettable);
}

// A named set is a live rule dependency: element updates must change packet
// matching, while deleting a set still referenced by a rule must fail.
TEST(NetlinkNetfilter, NftNamedIpv4SetLookupTracksElementUpdates) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd receiver(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd sender(socket(AF_INET, SOCK_DGRAM, 0));
  Fd nft(Open());
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(sender.get(), 0);
  ASSERT_GE(nft.get(), 0);
  sockaddr_in address{};
  address.sin_family = AF_INET;
  ASSERT_EQ(1, inet_pton(AF_INET, "127.0.0.1", &address.sin_addr));
  ASSERT_EQ(0, bind(receiver.get(), reinterpret_cast<sockaddr*>(&address),
                    sizeof(address)));
  socklen_t address_len = sizeof(address);
  ASSERT_EQ(0, getsockname(receiver.get(), reinterpret_cast<sockaddr*>(&address),
                           &address_len));

  constexpr uint32_t seq = 19000;
  constexpr char table_name[] = "dkc035_named_set";
  constexpr char set_name[] = "sources";
  in_addr additional_address{};
  ASSERT_EQ(1, inet_pton(AF_INET, "127.0.0.2", &additional_address));
  auto batch = TableBatch(kNftNewtable, seq, table_name);
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto set = Ipv4SourceSet(seq + 2, table_name, set_name);
  auto element = Ipv4SourceSetElement(kNftNewsetelem, seq + 3, table_name,
                                     set_name, address.sin_addr, nullptr,
                                     &additional_address);
  auto chain = NewChain(seq + 4, table_name, "input", true, NF_ACCEPT,
                        NF_INET_LOCAL_IN);
  auto rule = Ipv4SourceSetCounterRule(seq + 5, table_name, "input", set_name);
  auto end = Request(NFNL_MSG_BATCH_END, seq + 6);
  batch.insert(batch.end(), set.begin(), set.end());
  batch.insert(batch.end(), element.begin(), element.end());
  batch.insert(batch.end(), chain.begin(), chain.end());
  batch.insert(batch.end(), rule.begin(), rule.end());
  batch.insert(batch.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewset);
  Ack(nft.get(), seq + 3, 0, kNftNewsetelem);
  Ack(nft.get(), seq + 4, 0, kNftNewchain);
  Ack(nft.get(), seq + 5, 0, kNftNewrule);

  auto query = Request(kNftGetrule, seq + 7);
  query[NLMSG_HDRLEN] = NFPROTO_IPV4;
  nlmsghdr header{};
  std::memcpy(&header, query.data(), sizeof(header));
  header.nlmsg_flags |= NLM_F_DUMP;
  std::memcpy(query.data(), &header, sizeof(header));
  AppendAttr(&query, NFTA_RULE_TABLE, table_name);
  AppendAttr(&query, NFTA_RULE_CHAIN, "input");
  const uint8_t probe = 35;
  ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                      reinterpret_cast<sockaddr*>(&address), address_len));
  pollfd ready{receiver.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 2000));
  uint8_t received = 0;
  ASSERT_EQ(1, recv(receiver.get(), &received, 1, 0));
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
  const auto before = ReceiveRuleDump(nft.get(), seq + 7);
  ASSERT_EQ(1u, before.size());
  ASSERT_TRUE(before[0].has_counter);
  EXPECT_GE(before[0].packets, 1u);

  auto mutation = Request(NFNL_MSG_BATCH_BEGIN, seq + 8);
  nfgenmsg gen{};
  gen.res_id = htons(NFNL_SUBSYS_NFTABLES);
  std::memcpy(mutation.data() + NLMSG_HDRLEN, &gen, sizeof(gen));
  element = Ipv4SourceSetElement(kNftDelsetelem, seq + 9, table_name,
                                 set_name, address.sin_addr);
  end = Request(NFNL_MSG_BATCH_END, seq + 10);
  mutation.insert(mutation.end(), element.begin(), element.end());
  mutation.insert(mutation.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(mutation.size()), Send(nft.get(), mutation));
  Ack(nft.get(), seq + 9, 0, kNftDelsetelem);

  ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                      reinterpret_cast<sockaddr*>(&address), address_len));
  ready.revents = 0;
  ASSERT_EQ(1, poll(&ready, 1, 2000));
  ASSERT_EQ(1, recv(receiver.get(), &received, 1, 0));
  std::memcpy(&header, query.data(), sizeof(header));
  header.nlmsg_seq = seq + 11;
  std::memcpy(query.data(), &header, sizeof(header));
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
  const auto after = ReceiveRuleDump(nft.get(), seq + 11);
  ASSERT_EQ(1u, after.size());
  ASSERT_TRUE(after[0].has_counter);
  EXPECT_EQ(before[0].packets, after[0].packets);

  mutation = Request(NFNL_MSG_BATCH_BEGIN, seq + 12);
  std::memcpy(mutation.data() + NLMSG_HDRLEN, &gen, sizeof(gen));
  auto remove_set = Request(kNftDelset, seq + 13);
  remove_set[NLMSG_HDRLEN] = NFPROTO_IPV4;
  AppendAttr(&remove_set, NFTA_SET_TABLE, table_name);
  AppendAttr(&remove_set, NFTA_SET_NAME, set_name);
  end = Request(NFNL_MSG_BATCH_END, seq + 14);
  mutation.insert(mutation.end(), remove_set.begin(), remove_set.end());
  mutation.insert(mutation.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(mutation.size()), Send(nft.get(), mutation));
  Ack(nft.get(), seq + 13, EBUSY, kNftDelset);

  auto remove_table = TableBatch(kNftDeltable, seq + 15, table_name);
  ASSERT_EQ(static_cast<ssize_t>(remove_table.size()), Send(nft.get(), remove_table));
  Ack(nft.get(), seq + 16, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, NftIntervalGetReturnsStoredBoundaries) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  ASSERT_GE(nft.get(), 0);
  constexpr char table[] = "dkc035_interval_get";
  constexpr char name[] = "ranges";
  constexpr uint32_t seq = 29800;
  auto element_request = [&](uint16_t type, uint32_t sequence,
                             const std::vector<std::pair<uint32_t, uint32_t>>& items) {
    auto request = Request(type, sequence);
    request[NLMSG_HDRLEN] = NFPROTO_IPV4;
    if (type == kNftNewsetelem) {
      auto* header = reinterpret_cast<nlmsghdr*>(request.data());
      header->nlmsg_flags |= NLM_F_CREATE | NLM_F_EXCL;
    }
    AppendAttr(&request, NFTA_SET_ELEM_LIST_TABLE, table);
    AppendAttr(&request, NFTA_SET_ELEM_LIST_SET, name);
    std::vector<uint8_t> elements;
    for (const auto& item : items) {
      std::vector<uint8_t> key, element;
      const uint32_t address = htonl(item.first);
      AppendNla(&key, NFTA_DATA_VALUE, reinterpret_cast<const uint8_t*>(&address), 4);
      AppendNla(&element, NFTA_SET_ELEM_KEY | NLA_F_NESTED, key.data(), key.size());
      if (item.second) {
        const uint32_t flags = htonl(item.second);
        AppendNla(&element, NFTA_SET_ELEM_FLAGS,
                  reinterpret_cast<const uint8_t*>(&flags), 4);
      }
      AppendNla(&elements, NFTA_LIST_ELEM | NLA_F_NESTED, element.data(), element.size());
    }
    AppendRawAttr(&request, NFTA_SET_ELEM_LIST_ELEMENTS | NLA_F_NESTED,
                  elements.data(), elements.size());
    return request;
  };
  auto batch = TableBatch(kNftNewtable, seq, table);
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto set = Ipv4SourceSet(seq + 2, table, name, NFT_SET_INTERVAL);
  auto elements = element_request(kNftNewsetelem, seq + 3,
      {{0x0a000000, 0}, {0x0b000000, 1}, {0x0b000000, 0}, {0x0c000000, 1}});
  auto end = Request(NFNL_MSG_BATCH_END, seq + 4);
  batch.insert(batch.end(), set.begin(), set.end());
  batch.insert(batch.end(), elements.begin(), elements.end());
  batch.insert(batch.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewset);
  Ack(nft.get(), seq + 3, 0, kNftNewsetelem);
  // Independent queries prevent a pending ACK from masking a missing result.
  auto query = [&](uint32_t key, uint32_t flags, uint32_t expected) {
    Fd reader(Open());
    ASSERT_GE(reader.get(), 0);
    auto request = element_request(kNftGetsetelem, seq + 5, {{key, flags}});
    ASSERT_EQ(static_cast<ssize_t>(request.size()), Send(reader.get(), request));
    pollfd ready{reader.get(), POLLIN, 0};
    ASSERT_EQ(1, poll(&ready, 1, 2000));
    std::array<uint8_t, 4096> bytes{};
    const ssize_t count = recv(reader.get(), bytes.data(), bytes.size(), 0);
    ASSERT_GE(count, static_cast<ssize_t>(NLMSG_HDRLEN + sizeof(nfgenmsg)));
    auto* message = reinterpret_cast<nlmsghdr*>(bytes.data());
    ASSERT_EQ(kNftNewsetelem, message->nlmsg_type);
    const auto attrs = FindNla(
        {reinterpret_cast<const uint8_t*>(NLMSG_DATA(message)) + sizeof(nfgenmsg),
         static_cast<size_t>(NLMSG_PAYLOAD(message, sizeof(nfgenmsg)))},
        NFTA_SET_ELEM_LIST_ELEMENTS);
    const auto element = FindNla(attrs, NFTA_LIST_ELEM);
    const auto stored = FindNla(FindNla(element, NFTA_SET_ELEM_KEY), NFTA_DATA_VALUE);
    ASSERT_EQ(4u, stored.length);
    uint32_t address;
    std::memcpy(&address, stored.data, 4);
    EXPECT_EQ(expected, ntohl(address));
    const auto stored_flags = FindNla(element, NFTA_SET_ELEM_FLAGS);
    uint32_t returned_flags = 0;
    if (stored_flags.length == 4) std::memcpy(&returned_flags, stored_flags.data, 4);
    EXPECT_EQ(flags, ntohl(returned_flags));
  };
  query(0x0a010001, 0, 0x0a000000);
  query(0x0a010001, 1, 0x0b000000);
  query(0x0b000000, 0, 0x0b000000);
  query(0x0b000000, 1, 0x0b000000);
  query(0x0b010001, 0, 0x0b000000);
  query(0x0b010001, 1, 0x0c000000);
  auto remove = TableBatch(kNftDeltable, seq + 6, table);
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
  Ack(nft.get(), seq + 7, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, NftSetElementBatchFailureDoesNotPublishEarlierUpdate) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  ASSERT_GE(nft.get(), 0);
  constexpr uint32_t seq = 19200;
  constexpr char table[] = "dkc035_set_batch";
  constexpr char set_name[] = "sources";
  in_addr first{}, second{};
  ASSERT_EQ(1, inet_pton(AF_INET, "127.0.0.1", &first));
  ASSERT_EQ(1, inet_pton(AF_INET, "127.0.0.2", &second));

  auto create = TableBatch(kNftNewtable, seq, table);
  create.resize(create.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto set = Ipv4SourceSet(seq + 2, table, set_name);
  auto end = Request(NFNL_MSG_BATCH_END, seq + 3);
  create.insert(create.end(), set.begin(), set.end());
  create.insert(create.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(create.size()), Send(nft.get(), create));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewset);

  auto make_batch = [&](uint32_t begin_seq, in_addr first_key, in_addr second_key) {
    auto batch = Request(NFNL_MSG_BATCH_BEGIN, begin_seq);
    nfgenmsg gen{};
    gen.res_id = htons(NFNL_SUBSYS_NFTABLES);
    std::memcpy(batch.data() + NLMSG_HDRLEN, &gen, sizeof(gen));
    auto first_element = Ipv4SourceSetElement(kNftNewsetelem, begin_seq + 1,
                                               table, set_name, first_key);
    auto second_element = Ipv4SourceSetElement(kNftNewsetelem, begin_seq + 2,
                                                table, set_name, second_key);
    auto batch_end = Request(NFNL_MSG_BATCH_END, begin_seq + 3);
    batch.insert(batch.end(), first_element.begin(), first_element.end());
    batch.insert(batch.end(), second_element.begin(), second_element.end());
    batch.insert(batch.end(), batch_end.begin(), batch_end.end());
    return batch;
  };

  auto failed = make_batch(seq + 4, first, first);
  ASSERT_EQ(static_cast<ssize_t>(failed.size()), Send(nft.get(), failed));
  Ack(nft.get(), seq + 5, 0, kNftNewsetelem);
  Ack(nft.get(), seq + 6, EEXIST, kNftNewsetelem);

  auto dump_count = [&](uint32_t query_seq) {
    auto query = Request(kNftGetsetelem, query_seq);
    query[NLMSG_HDRLEN] = NFPROTO_IPV4;
    nlmsghdr header{};
    std::memcpy(&header, query.data(), sizeof(header));
    header.nlmsg_flags |= NLM_F_DUMP;
    std::memcpy(query.data(), &header, sizeof(header));
    AppendAttr(&query, NFTA_SET_ELEM_LIST_TABLE, table);
    AppendAttr(&query, NFTA_SET_ELEM_LIST_SET, set_name);
    EXPECT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
    return ReceiveSetElementDumpCount(nft.get(), query_seq);
  };
  EXPECT_EQ(0u, dump_count(seq + 8));

  auto committed = make_batch(seq + 9, first, second);
  ASSERT_EQ(static_cast<ssize_t>(committed.size()), Send(nft.get(), committed));
  Ack(nft.get(), seq + 10, 0, kNftNewsetelem);
  Ack(nft.get(), seq + 11, 0, kNftNewsetelem);
  EXPECT_EQ(2u, dump_count(seq + 13));

  auto remove = TableBatch(kNftDeltable, seq + 14, table);
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
  Ack(nft.get(), seq + 15, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, NftByteorderAndRangeExpressionsMatchRealPacket) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  Fd receiver(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd sender(socket(AF_INET, SOCK_DGRAM, 0));
  ASSERT_GE(nft.get(), 0);
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(sender.get(), 0);
  sockaddr_in address{};
  address.sin_family = AF_INET;
  ASSERT_EQ(1, inet_pton(AF_INET, "127.0.0.1", &address.sin_addr));
  ASSERT_EQ(0, bind(receiver.get(), reinterpret_cast<sockaddr*>(&address),
                    sizeof(address)));
  socklen_t address_len = sizeof(address);
  ASSERT_EQ(0, getsockname(receiver.get(), reinterpret_cast<sockaddr*>(&address),
                           &address_len));

  constexpr uint32_t seq = 19300;
  constexpr char table[] = "dkc035_vm_ops";
  in_addr first{}, last{};
  ASSERT_EQ(1, inet_pton(AF_INET, "127.0.0.2", &first));
  ASSERT_EQ(1, inet_pton(AF_INET, "127.0.0.3", &last));
  auto create = TableBatch(kNftNewtable, seq, table);
  create.resize(create.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto chain = NewChain(seq + 2, table, "input", true, NF_ACCEPT,
                        NF_INET_LOCAL_IN);
  auto byteorder = PacketLengthCounterRule(seq + 3, table, "input");
  auto range = Ipv4SourceNotRangeCounterRule(seq + 4, table, "input", first, last);
  auto end = Request(NFNL_MSG_BATCH_END, seq + 5);
  create.insert(create.end(), chain.begin(), chain.end());
  create.insert(create.end(), byteorder.begin(), byteorder.end());
  create.insert(create.end(), range.begin(), range.end());
  create.insert(create.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(create.size()), Send(nft.get(), create));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewchain);
  Ack(nft.get(), seq + 3, 0, kNftNewrule);
  Ack(nft.get(), seq + 4, 0, kNftNewrule);

  const uint8_t probe = 35;
  ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                      reinterpret_cast<sockaddr*>(&address), address_len));
  pollfd ready{receiver.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 2000));
  uint8_t received = 0;
  ASSERT_EQ(1, recv(receiver.get(), &received, 1, 0));

  auto query = Request(kNftGetrule, seq + 6);
  query[NLMSG_HDRLEN] = NFPROTO_IPV4;
  nlmsghdr header{};
  std::memcpy(&header, query.data(), sizeof(header));
  header.nlmsg_flags |= NLM_F_DUMP;
  std::memcpy(query.data(), &header, sizeof(header));
  AppendAttr(&query, NFTA_RULE_TABLE, table);
  AppendAttr(&query, NFTA_RULE_CHAIN, "input");
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
  const auto rules = ReceiveRuleDump(nft.get(), seq + 6);
  ASSERT_EQ(2u, rules.size());
  EXPECT_TRUE(rules[0].has_counter);
  EXPECT_TRUE(rules[1].has_counter);
  EXPECT_EQ(1u, rules[0].packets);
  EXPECT_EQ(1u, rules[1].packets);

  auto remove = TableBatch(kNftDeltable, seq + 7, table);
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
  Ack(nft.get(), seq + 8, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, NftInetMetaProtocolAndFamilyMatchRealIpv4Packet) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  Fd receiver(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd sender(socket(AF_INET, SOCK_DGRAM, 0));
  ASSERT_GE(nft.get(), 0);
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(sender.get(), 0);
  sockaddr_in address{};
  address.sin_family = AF_INET;
  ASSERT_EQ(1, inet_pton(AF_INET, "127.0.0.1", &address.sin_addr));
  ASSERT_EQ(0, bind(receiver.get(), reinterpret_cast<sockaddr*>(&address),
                    sizeof(address)));
  socklen_t address_len = sizeof(address);
  ASSERT_EQ(0, getsockname(receiver.get(), reinterpret_cast<sockaddr*>(&address),
                           &address_len));

  constexpr uint32_t seq = 19400;
  constexpr char table[] = "dkc035_meta_read";
  const uint8_t ipv4_family = NFPROTO_IPV4;
  const uint16_t ipv4_protocol = htons(ETH_P_IP);
  auto create = TableBatch(kNftNewtable, seq, table, NFPROTO_INET);
  create.resize(create.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto chain = NewChain(seq + 2, table, "input", true, NF_ACCEPT,
                        NF_INET_LOCAL_IN, NFPROTO_INET);
  auto family = MetaCounterRule(seq + 3, table, "input", NFT_META_NFPROTO,
                                &ipv4_family, 1, NFPROTO_INET);
  auto protocol = MetaCounterRule(seq + 4, table, "input", NFT_META_PROTOCOL,
                                  reinterpret_cast<const uint8_t*>(&ipv4_protocol),
                                  2, NFPROTO_INET);
  auto end = Request(NFNL_MSG_BATCH_END, seq + 5);
  create.insert(create.end(), chain.begin(), chain.end());
  create.insert(create.end(), family.begin(), family.end());
  create.insert(create.end(), protocol.begin(), protocol.end());
  create.insert(create.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(create.size()), Send(nft.get(), create));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewchain);
  Ack(nft.get(), seq + 3, 0, kNftNewrule);
  Ack(nft.get(), seq + 4, 0, kNftNewrule);

  const uint8_t probe = 35;
  ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                      reinterpret_cast<sockaddr*>(&address), address_len));
  pollfd ready{receiver.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 2000));
  uint8_t received = 0;
  ASSERT_EQ(1, recv(receiver.get(), &received, 1, 0));
  auto query = Request(kNftGetrule, seq + 6);
  query[NLMSG_HDRLEN] = NFPROTO_INET;
  nlmsghdr header{};
  std::memcpy(&header, query.data(), sizeof(header));
  header.nlmsg_flags |= NLM_F_DUMP;
  std::memcpy(query.data(), &header, sizeof(header));
  AppendAttr(&query, NFTA_RULE_TABLE, table);
  AppendAttr(&query, NFTA_RULE_CHAIN, "input");
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
  const auto rules = ReceiveRuleDump(nft.get(), seq + 6);
  ASSERT_EQ(2u, rules.size());
  EXPECT_EQ(1u, rules[0].packets);
  EXPECT_EQ(1u, rules[1].packets);

  auto remove = TableBatch(kNftDeltable, seq + 7, table, NFPROTO_INET);
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
  Ack(nft.get(), seq + 8, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, NftOutputMarkPersistsAcrossLoopbackInputHook) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  Fd receiver(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd sender(socket(AF_INET, SOCK_DGRAM, 0));
  ASSERT_GE(nft.get(), 0);
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(sender.get(), 0);
  sockaddr_in address{};
  address.sin_family = AF_INET;
  ASSERT_EQ(1, inet_pton(AF_INET, "127.0.0.1", &address.sin_addr));
  ASSERT_EQ(0, bind(receiver.get(), reinterpret_cast<sockaddr*>(&address),
                    sizeof(address)));
  socklen_t address_len = sizeof(address);
  ASSERT_EQ(0, getsockname(receiver.get(), reinterpret_cast<sockaddr*>(&address),
                           &address_len));

  constexpr uint32_t seq = 19500;
  constexpr char table[] = "dkc035_mark_flow";
  const uint32_t mark = 0x35;
  auto create = TableBatch(kNftNewtable, seq, table);
  create.resize(create.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto output = NewChain(seq + 2, table, "output", true, NF_ACCEPT,
                         NF_INET_LOCAL_OUT);
  auto input = NewChain(seq + 3, table, "input", true, NF_ACCEPT,
                        NF_INET_LOCAL_IN);
  auto set_mark = MetaMarkSetRule(seq + 4, table, "output", mark);
  auto match_mark = MetaCounterRule(seq + 5, table, "input", NFT_META_MARK,
                                    reinterpret_cast<const uint8_t*>(&mark),
                                    sizeof(mark));
  auto end = Request(NFNL_MSG_BATCH_END, seq + 6);
  create.insert(create.end(), output.begin(), output.end());
  create.insert(create.end(), input.begin(), input.end());
  create.insert(create.end(), set_mark.begin(), set_mark.end());
  create.insert(create.end(), match_mark.begin(), match_mark.end());
  create.insert(create.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(create.size()), Send(nft.get(), create));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewchain);
  Ack(nft.get(), seq + 3, 0, kNftNewchain);
  Ack(nft.get(), seq + 4, 0, kNftNewrule);
  Ack(nft.get(), seq + 5, 0, kNftNewrule);

  const uint8_t probe = 35;
  ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                      reinterpret_cast<sockaddr*>(&address), address_len));
  pollfd ready{receiver.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 2000));
  uint8_t received = 0;
  ASSERT_EQ(1, recv(receiver.get(), &received, 1, 0));
  auto query = Request(kNftGetrule, seq + 7);
  query[NLMSG_HDRLEN] = NFPROTO_IPV4;
  nlmsghdr header{};
  std::memcpy(&header, query.data(), sizeof(header));
  header.nlmsg_flags |= NLM_F_DUMP;
  std::memcpy(query.data(), &header, sizeof(header));
  AppendAttr(&query, NFTA_RULE_TABLE, table);
  AppendAttr(&query, NFTA_RULE_CHAIN, "input");
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
  const auto rules = ReceiveRuleDump(nft.get(), seq + 7);
  ASSERT_EQ(1u, rules.size());
  EXPECT_TRUE(rules[0].has_counter);
  EXPECT_EQ(1u, rules[0].packets);

  auto remove = TableBatch(kNftDeltable, seq + 8, table);
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
  Ack(nft.get(), seq + 9, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, NftFibDestinationAddrtypeMatchesRealLocalPacket) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  Fd receiver(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd sender(socket(AF_INET, SOCK_DGRAM, 0));
  ASSERT_GE(nft.get(), 0);
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(sender.get(), 0);
  sockaddr_in address{};
  address.sin_family = AF_INET;
  ASSERT_EQ(1, inet_pton(AF_INET, "127.0.0.1", &address.sin_addr));
  ASSERT_EQ(0, bind(receiver.get(), reinterpret_cast<sockaddr*>(&address),
                    sizeof(address)));
  socklen_t address_len = sizeof(address);
  ASSERT_EQ(0, getsockname(receiver.get(), reinterpret_cast<sockaddr*>(&address),
                           &address_len));

  constexpr uint32_t seq = 19700;
  constexpr char table[] = "dkc035_fib_packet";
  auto create = TableBatch(kNftNewtable, seq, table);
  create.resize(create.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto input = NewChain(seq + 2, table, "input", true, NF_ACCEPT,
                        NF_INET_LOCAL_IN);
  auto rule = FibIpv4DestinationLocalCounterRule(seq + 3, table, "input");
  auto end = Request(NFNL_MSG_BATCH_END, seq + 4);
  create.insert(create.end(), input.begin(), input.end());
  create.insert(create.end(), rule.begin(), rule.end());
  create.insert(create.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(create.size()), Send(nft.get(), create));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewchain);
  Ack(nft.get(), seq + 3, 0, kNftNewrule);

  const uint8_t probe = 37;
  ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                      reinterpret_cast<sockaddr*>(&address), address_len));
  pollfd ready{receiver.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 2000));
  uint8_t received = 0;
  ASSERT_EQ(1, recv(receiver.get(), &received, 1, 0));
  EXPECT_EQ(probe, received);

  auto query = Request(kNftGetrule, seq + 5);
  query[NLMSG_HDRLEN] = NFPROTO_IPV4;
  nlmsghdr header{};
  std::memcpy(&header, query.data(), sizeof(header));
  header.nlmsg_flags |= NLM_F_DUMP;
  std::memcpy(query.data(), &header, sizeof(header));
  AppendAttr(&query, NFTA_RULE_TABLE, table);
  AppendAttr(&query, NFTA_RULE_CHAIN, "input");
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
  const auto rules = ReceiveRuleDump(nft.get(), seq + 5);
  ASSERT_EQ(1u, rules.size());
  EXPECT_TRUE(rules[0].has_counter);
  EXPECT_EQ(1u, rules[0].packets);

  auto remove = TableBatch(kNftDeltable, seq + 6, table);
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
  Ack(nft.get(), seq + 7, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, NftIpv4DataMapValueFeedsRuleRegister) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd receiver(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd sender(socket(AF_INET, SOCK_DGRAM, 0));
  Fd nft(Open());
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(sender.get(), 0);
  ASSERT_GE(nft.get(), 0);
  sockaddr_in address{};
  address.sin_family = AF_INET;
  ASSERT_EQ(1, inet_pton(AF_INET, "127.0.0.1", &address.sin_addr));
  ASSERT_EQ(0, bind(receiver.get(), reinterpret_cast<sockaddr*>(&address),
                    sizeof(address)));
  socklen_t address_len = sizeof(address);
  ASSERT_EQ(0, getsockname(receiver.get(), reinterpret_cast<sockaddr*>(&address),
                           &address_len));
  in_addr mapped{};
  ASSERT_EQ(1, inet_pton(AF_INET, "127.0.0.2", &mapped));

  constexpr uint32_t seq = 19100;
  constexpr char table_name[] = "dkc035_data_map";
  constexpr char map_name[] = "rewrite";
  auto batch = TableBatch(kNftNewtable, seq, table_name);
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto map = Ipv4SourceSet(seq + 2, table_name, map_name, NFT_SET_MAP);
  auto element = Ipv4SourceSetElement(kNftNewsetelem, seq + 3, table_name,
                                     map_name, address.sin_addr, &mapped);
  auto chain = NewChain(seq + 4, table_name, "input", true, NF_ACCEPT,
                        NF_INET_LOCAL_IN);
  auto rule = Ipv4SourceMapCounterRule(seq + 5, table_name, "input",
                                      map_name, mapped);
  auto end = Request(NFNL_MSG_BATCH_END, seq + 6);
  batch.insert(batch.end(), map.begin(), map.end());
  batch.insert(batch.end(), element.begin(), element.end());
  batch.insert(batch.end(), chain.begin(), chain.end());
  batch.insert(batch.end(), rule.begin(), rule.end());
  batch.insert(batch.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewset);
  Ack(nft.get(), seq + 3, 0, kNftNewsetelem);
  Ack(nft.get(), seq + 4, 0, kNftNewchain);
  Ack(nft.get(), seq + 5, 0, kNftNewrule);

  const uint8_t probe = 36;
  ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                      reinterpret_cast<sockaddr*>(&address), address_len));
  pollfd ready{receiver.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 2000));
  uint8_t received = 0;
  ASSERT_EQ(1, recv(receiver.get(), &received, 1, 0));

  auto query = Request(kNftGetrule, seq + 7);
  query[NLMSG_HDRLEN] = NFPROTO_IPV4;
  nlmsghdr header{};
  std::memcpy(&header, query.data(), sizeof(header));
  header.nlmsg_flags |= NLM_F_DUMP;
  std::memcpy(query.data(), &header, sizeof(header));
  AppendAttr(&query, NFTA_RULE_TABLE, table_name);
  AppendAttr(&query, NFTA_RULE_CHAIN, "input");
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
  const auto rules = ReceiveRuleDump(nft.get(), seq + 7);
  ASSERT_EQ(1u, rules.size());
  ASSERT_TRUE(rules[0].has_counter);
  EXPECT_GE(rules[0].packets, 1u);

  auto remove = TableBatch(kNftDeltable, seq + 8, table_name);
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
  Ack(nft.get(), seq + 9, 0, kNftDeltable);
}

// The filter must affect real packets, not merely acknowledge NEWCHAIN.
TEST(NetlinkNetfilter, NftOrderedNetworkMatchAndCounterGatePackets) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd receiver(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd sender(socket(AF_INET, SOCK_DGRAM, 0));
  Fd nft(Open());
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(sender.get(), 0);
  ASSERT_GE(nft.get(), 0);
  sockaddr_in address{};
  address.sin_family = AF_INET;
  address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  ASSERT_EQ(0, bind(receiver.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)));
  socklen_t address_len = sizeof(address);
  ASSERT_EQ(0, getsockname(receiver.get(), reinterpret_cast<sockaddr*>(&address), &address_len));

  constexpr uint32_t seq = 990;
  auto batch = TableBatch(kNftNewtable, seq, "dkc035_expr");
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto chain = NewChain(seq + 2, "dkc035_expr", "prerouting", true);
  auto mismatch = ProtocolCounterRule(seq + 3, "dkc035_expr", "prerouting", NFT_CMP_NEQ);
  auto end = Request(NFNL_MSG_BATCH_END, seq + 4);
  batch.insert(batch.end(), chain.begin(), chain.end());
  batch.insert(batch.end(), mismatch.begin(), mismatch.end());
  batch.insert(batch.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewchain);
  Ack(nft.get(), seq + 3, 0, kNftNewrule);

  const uint8_t probe = 19;
  ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                      reinterpret_cast<sockaddr*>(&address), address_len));
  pollfd ready{receiver.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 2000));
  uint8_t received = 0;
  ASSERT_EQ(1, recv(receiver.get(), &received, 1, 0));
  EXPECT_EQ(probe, received);

  auto add = Request(NFNL_MSG_BATCH_BEGIN, seq + 5);
  nfgenmsg gen{};
  gen.res_id = htons(NFNL_SUBSYS_NFTABLES);
  std::memcpy(add.data() + NLMSG_HDRLEN, &gen, sizeof(gen));
  auto match = ProtocolCounterRule(seq + 6, "dkc035_expr", "prerouting", NFT_CMP_EQ);
  end = Request(NFNL_MSG_BATCH_END, seq + 7);
  add.insert(add.end(), match.begin(), match.end());
  add.insert(add.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(add.size()), Send(nft.get(), add));
  Ack(nft.get(), seq + 6, 0, kNftNewrule);

  ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                      reinterpret_cast<sockaddr*>(&address), address_len));
  ready.revents = 0;
  EXPECT_EQ(0, poll(&ready, 1, 150));

  auto query = Request(kNftGetrule, seq + 8);
  query[NLMSG_HDRLEN] = NFPROTO_IPV4;
  nlmsghdr header{};
  std::memcpy(&header, query.data(), sizeof(header));
  header.nlmsg_flags |= NLM_F_DUMP;
  std::memcpy(query.data(), &header, sizeof(header));
  AppendAttr(&query, NFTA_RULE_TABLE, "dkc035_expr");
  AppendAttr(&query, NFTA_RULE_CHAIN, "prerouting");
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
  const auto counted = ReceiveRuleDump(nft.get(), seq + 8);
  ASSERT_EQ(2u, counted.size());
  EXPECT_TRUE(counted[0].has_counter);
  EXPECT_EQ(0u, counted[0].packets);
  EXPECT_TRUE(counted[1].has_counter);
  EXPECT_GE(counted[1].packets, 1u);
  EXPECT_GE(counted[1].bytes, 29u);

  auto mutate = Request(NFNL_MSG_BATCH_BEGIN, seq + 10);
  std::memcpy(mutate.data() + NLMSG_HDRLEN, &gen, sizeof(gen));
  auto extra = NewChain(seq + 11, "dkc035_expr", "extra");
  end = Request(NFNL_MSG_BATCH_END, seq + 12);
  mutate.insert(mutate.end(), extra.begin(), extra.end());
  mutate.insert(mutate.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(mutate.size()), Send(nft.get(), mutate));
  Ack(nft.get(), seq + 11, 0, kNftNewchain);

  ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                      reinterpret_cast<sockaddr*>(&address), address_len));
  ready.revents = 0;
  EXPECT_EQ(0, poll(&ready, 1, 150));
  std::memcpy(&header, query.data(), sizeof(header));
  header.nlmsg_seq = seq + 13;
  std::memcpy(query.data(), &header, sizeof(header));
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
  const auto after_cow = ReceiveRuleDump(nft.get(), seq + 13);
  ASSERT_EQ(2u, after_cow.size());
  EXPECT_TRUE(after_cow[1].has_counter);
  EXPECT_EQ(counted[1].packets + 1, after_cow[1].packets);
  EXPECT_GT(after_cow[1].bytes, counted[1].bytes);

  auto remove = TableBatch(kNftDeltable, seq + 14, "dkc035_expr");
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
  Ack(nft.get(), seq + 15, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, NftMetaUsesRealHookNamesAndZeroPaddedL4Protocol) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  Fd receiver(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd sender(socket(AF_INET, SOCK_DGRAM, 0));
  ASSERT_GE(nft.get(), 0);
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(sender.get(), 0);
  sockaddr_in address{};
  address.sin_family = AF_INET;
  address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  ASSERT_EQ(0, bind(receiver.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)));
  socklen_t address_len = sizeof(address);
  ASSERT_EQ(0, getsockname(receiver.get(), reinterpret_cast<sockaddr*>(&address), &address_len));

  const std::string table = "dkc035_meta_" + std::to_string(getpid());
  constexpr uint32_t seq = 1340;
  auto batch = TableBatch(kNftNewtable, seq, table.c_str());
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto prerouting = NewChain(seq + 2, table.c_str(), "prerouting", true);
  auto input = NewChain(seq + 3, table.c_str(), "input", true, NF_ACCEPT,
                        NF_INET_LOCAL_IN);
  const std::array<uint8_t, IFNAMSIZ> empty_name{};
  std::array<uint8_t, IFNAMSIZ> loopback_name{};
  std::memcpy(loopback_name.data(), "lo", 2);
  const std::array<uint8_t, 4> udp_protocol{IPPROTO_UDP, 0, 0, 0};
  auto pre_no_oif = MetaCounterRule(seq + 4, table.c_str(), "prerouting",
                                    NFT_META_OIFNAME, empty_name.data(), empty_name.size());
  auto input_iif = MetaCounterRule(seq + 5, table.c_str(), "input",
                                   NFT_META_IIFNAME, loopback_name.data(), loopback_name.size());
  auto input_proto = MetaCounterRule(seq + 6, table.c_str(), "input",
                                     NFT_META_L4PROTO, udp_protocol.data(), udp_protocol.size());
  for (const auto* message : {&prerouting, &input, &pre_no_oif, &input_iif, &input_proto})
    batch.insert(batch.end(), message->begin(), message->end());
  auto end = Request(NFNL_MSG_BATCH_END, seq + 7);
  batch.insert(batch.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewchain);
  Ack(nft.get(), seq + 3, 0, kNftNewchain);
  for (uint32_t offset = 4; offset <= 6; ++offset)
    Ack(nft.get(), seq + offset, 0, kNftNewrule);

  const uint8_t probe = 42;
  ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                      reinterpret_cast<sockaddr*>(&address), address_len));
  pollfd ready{receiver.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 1000));
  uint8_t received = 0;
  ASSERT_EQ(1, recv(receiver.get(), &received, 1, 0));
  EXPECT_EQ(probe, received);

  auto count = [&](const char* chain, uint32_t query_seq,
                   std::initializer_list<uint32_t> expected_keys) {
    auto query = Request(kNftGetrule, query_seq);
    query[NLMSG_HDRLEN] = NFPROTO_IPV4;
    nlmsghdr header{};
    std::memcpy(&header, query.data(), sizeof(header));
    header.nlmsg_flags |= NLM_F_DUMP;
    std::memcpy(query.data(), &header, sizeof(header));
    AppendAttr(&query, NFTA_RULE_TABLE, table.c_str());
    AppendAttr(&query, NFTA_RULE_CHAIN, chain);
    EXPECT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
    const auto rules = ReceiveRuleDump(nft.get(), query_seq);
    EXPECT_EQ(expected_keys.size(), rules.size());
    size_t index = 0;
    for (const auto& rule : rules) {
      EXPECT_TRUE(rule.has_counter);
      EXPECT_GE(rule.packets, 1u);
      EXPECT_TRUE(rule.valid_meta_dump);
      if (index < expected_keys.size()) {
        EXPECT_EQ((std::vector<uint32_t>{*(expected_keys.begin() + index)}), rule.meta_keys);
      }
      ++index;
    }
  };
  count("prerouting", seq + 8, {NFT_META_OIFNAME});
  count("input", seq + 9, {NFT_META_IIFNAME, NFT_META_L4PROTO});
  auto remove = TableBatch(kNftDeltable, seq + 10, table.c_str());
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
  Ack(nft.get(), seq + 11, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, NftLocalOutputNamesLoopbackNotAddressOwner) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  ASSERT_NE(0u, if_nametoindex("veth2")) << "requires the network test fixture";
  Fd nft(Open());
  Fd receiver(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd sender(socket(AF_INET, SOCK_DGRAM, 0));
  ASSERT_GE(nft.get(), 0);
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(sender.get(), 0);
  sockaddr_in address{};
  address.sin_family = AF_INET;
  address.sin_addr.s_addr = inet_addr("111.111.11.2");
  ASSERT_EQ(0, bind(receiver.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)));
  socklen_t address_len = sizeof(address);
  ASSERT_EQ(0, getsockname(receiver.get(), reinterpret_cast<sockaddr*>(&address), &address_len));

  const std::string table = "dkc035_local_oif_" + std::to_string(getpid());
  constexpr uint32_t seq = 1360;
  auto batch = TableBatch(kNftNewtable, seq, table.c_str());
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto output = NewChain(seq + 2, table.c_str(), "output", true, NF_ACCEPT,
                         NF_INET_LOCAL_OUT);
  std::array<uint8_t, IFNAMSIZ> loopback_name{}, owner_name{};
  std::memcpy(loopback_name.data(), "lo", 2);
  std::memcpy(owner_name.data(), "veth2", 5);
  auto loopback = MetaCounterRule(seq + 3, table.c_str(), "output", NFT_META_OIFNAME,
                                  loopback_name.data(), loopback_name.size());
  auto owner = MetaCounterRule(seq + 4, table.c_str(), "output", NFT_META_OIFNAME,
                               owner_name.data(), owner_name.size());
  for (const auto* message : {&output, &loopback, &owner})
    batch.insert(batch.end(), message->begin(), message->end());
  auto end = Request(NFNL_MSG_BATCH_END, seq + 5);
  batch.insert(batch.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewchain);
  Ack(nft.get(), seq + 3, 0, kNftNewrule);
  Ack(nft.get(), seq + 4, 0, kNftNewrule);

  const uint8_t probe = 0x35;
  ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                      reinterpret_cast<sockaddr*>(&address), address_len));
  pollfd ready{receiver.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 1000));
  uint8_t received = 0;
  ASSERT_EQ(1, recv(receiver.get(), &received, 1, 0));
  EXPECT_EQ(probe, received);

  auto query = Request(kNftGetrule, seq + 6);
  query[NLMSG_HDRLEN] = NFPROTO_IPV4;
  nlmsghdr header{};
  std::memcpy(&header, query.data(), sizeof(header));
  header.nlmsg_flags |= NLM_F_DUMP;
  std::memcpy(query.data(), &header, sizeof(header));
  AppendAttr(&query, NFTA_RULE_TABLE, table.c_str());
  AppendAttr(&query, NFTA_RULE_CHAIN, "output");
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
  const auto rules = ReceiveRuleDump(nft.get(), seq + 6);
  ASSERT_EQ(2u, rules.size());
  EXPECT_GE(rules[0].packets, 1u);
  EXPECT_EQ(0u, rules[1].packets);

  auto remove = TableBatch(kNftDeltable, seq + 7, table.c_str());
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
  Ack(nft.get(), seq + 8, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, NftCtStateTracksLocalUdpRequestAndReply) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  Fd client(socket(AF_INET, SOCK_DGRAM, 0));
  Fd server(socket(AF_INET, SOCK_DGRAM, 0));
  ASSERT_GE(nft.get(), 0);
  ASSERT_GE(client.get(), 0);
  ASSERT_GE(server.get(), 0);
  sockaddr_in client_address{}, server_address{};
  client_address.sin_family = server_address.sin_family = AF_INET;
  client_address.sin_addr.s_addr = server_address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  ASSERT_EQ(0, bind(client.get(), reinterpret_cast<sockaddr*>(&client_address),
                    sizeof(client_address)));
  ASSERT_EQ(0, bind(server.get(), reinterpret_cast<sockaddr*>(&server_address),
                    sizeof(server_address)));
  socklen_t address_len = sizeof(server_address);
  ASSERT_EQ(0, getsockname(server.get(), reinterpret_cast<sockaddr*>(&server_address),
                           &address_len));
  address_len = sizeof(client_address);
  ASSERT_EQ(0, getsockname(client.get(), reinterpret_cast<sockaddr*>(&client_address),
                           &address_len));

  const std::string table = "dkc035_ct_state_" + std::to_string(getpid());
  constexpr uint32_t seq = 1390;
  auto batch = TableBatch(kNftNewtable, seq, table.c_str());
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto input = NewChain(seq + 2, table.c_str(), "input", true, NF_ACCEPT,
                        NF_INET_LOCAL_IN);
  auto request = CtStateCounterRule(seq + 3, table.c_str(), "input", 1u << 3);
  auto reply = CtStateCounterRule(seq + 4, table.c_str(), "input", 1u << 1);
  for (const auto* message : {&input, &request, &reply})
    batch.insert(batch.end(), message->begin(), message->end());
  auto end = Request(NFNL_MSG_BATCH_END, seq + 5);
  batch.insert(batch.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewchain);
  Ack(nft.get(), seq + 3, 0, kNftNewrule);
  Ack(nft.get(), seq + 4, 0, kNftNewrule);

  const uint8_t probe = 0x35;
  ASSERT_EQ(1, sendto(client.get(), &probe, 1, 0,
                      reinterpret_cast<sockaddr*>(&server_address), sizeof(server_address)));
  uint8_t received = 0;
  pollfd server_ready{server.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&server_ready, 1, 1000));
  ASSERT_EQ(1, recv(server.get(), &received, 1, 0));
  EXPECT_EQ(probe, received);
  ASSERT_EQ(1, sendto(server.get(), &probe, 1, 0,
                      reinterpret_cast<sockaddr*>(&client_address), sizeof(client_address)));
  pollfd client_ready{client.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&client_ready, 1, 1000));
  ASSERT_EQ(1, recv(client.get(), &received, 1, 0));
  EXPECT_EQ(probe, received);

  auto query = Request(kNftGetrule, seq + 6);
  query[NLMSG_HDRLEN] = NFPROTO_IPV4;
  nlmsghdr header{};
  std::memcpy(&header, query.data(), sizeof(header));
  header.nlmsg_flags |= NLM_F_DUMP;
  std::memcpy(query.data(), &header, sizeof(header));
  AppendAttr(&query, NFTA_RULE_TABLE, table.c_str());
  AppendAttr(&query, NFTA_RULE_CHAIN, "input");
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
  const auto rules = ReceiveRuleDump(nft.get(), seq + 6);
  ASSERT_EQ(2u, rules.size());
  EXPECT_GE(rules[0].packets, 1u);
  EXPECT_GE(rules[1].packets, 1u);

  auto remove = TableBatch(kNftDeltable, seq + 7, table.c_str());
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
  Ack(nft.get(), seq + 8, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, NftIpv6CtStateTracksLocalUdpRequestAndReply) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  Fd client(socket(AF_INET6, SOCK_DGRAM, 0));
  Fd server(socket(AF_INET6, SOCK_DGRAM, 0));
  ASSERT_GE(nft.get(), 0);
  ASSERT_GE(client.get(), 0);
  ASSERT_GE(server.get(), 0);
  sockaddr_in6 client_address{}, server_address{};
  client_address.sin6_family = server_address.sin6_family = AF_INET6;
  client_address.sin6_addr = server_address.sin6_addr = in6addr_loopback;
  ASSERT_EQ(0, bind(client.get(), reinterpret_cast<sockaddr*>(&client_address),
                    sizeof(client_address)));
  ASSERT_EQ(0, bind(server.get(), reinterpret_cast<sockaddr*>(&server_address),
                    sizeof(server_address)));
  socklen_t address_len = sizeof(server_address);
  ASSERT_EQ(0, getsockname(server.get(), reinterpret_cast<sockaddr*>(&server_address),
                           &address_len));
  address_len = sizeof(client_address);
  ASSERT_EQ(0, getsockname(client.get(), reinterpret_cast<sockaddr*>(&client_address),
                           &address_len));

  const std::string table = "dkc035_ct6_state_" + std::to_string(getpid());
  constexpr uint32_t seq = 1410;
  auto batch = TableBatch(kNftNewtable, seq, table.c_str(), NFPROTO_IPV6);
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto input = NewChain(seq + 2, table.c_str(), "input", true, NF_ACCEPT,
                        NF_INET_LOCAL_IN, NFPROTO_IPV6);
  auto request = CtStateCounterRule(seq + 3, table.c_str(), "input", 1u << 3, NFPROTO_IPV6);
  auto reply = CtStateCounterRule(seq + 4, table.c_str(), "input", 1u << 1, NFPROTO_IPV6);
  for (const auto* message : {&input, &request, &reply})
    batch.insert(batch.end(), message->begin(), message->end());
  auto end = Request(NFNL_MSG_BATCH_END, seq + 5);
  batch.insert(batch.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewchain);
  Ack(nft.get(), seq + 3, 0, kNftNewrule);
  Ack(nft.get(), seq + 4, 0, kNftNewrule);

  const uint8_t probe = 0x36;
  ASSERT_EQ(1, sendto(client.get(), &probe, 1, 0,
                      reinterpret_cast<sockaddr*>(&server_address), sizeof(server_address)));
  uint8_t received = 0;
  pollfd server_ready{server.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&server_ready, 1, 1000));
  ASSERT_EQ(1, recv(server.get(), &received, 1, 0));
  EXPECT_EQ(probe, received);
  ASSERT_EQ(1, sendto(server.get(), &probe, 1, 0,
                      reinterpret_cast<sockaddr*>(&client_address), sizeof(client_address)));
  pollfd client_ready{client.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&client_ready, 1, 1000));
  ASSERT_EQ(1, recv(client.get(), &received, 1, 0));
  EXPECT_EQ(probe, received);

  auto query = Request(kNftGetrule, seq + 6);
  query[NLMSG_HDRLEN] = NFPROTO_IPV6;
  nlmsghdr header{};
  std::memcpy(&header, query.data(), sizeof(header));
  header.nlmsg_flags |= NLM_F_DUMP;
  std::memcpy(query.data(), &header, sizeof(header));
  AppendAttr(&query, NFTA_RULE_TABLE, table.c_str());
  AppendAttr(&query, NFTA_RULE_CHAIN, "input");
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
  const auto rules = ReceiveRuleDump(nft.get(), seq + 6);
  ASSERT_EQ(2u, rules.size());
  EXPECT_GE(rules[0].packets, 1u);
  EXPECT_GE(rules[1].packets, 1u);

  auto remove = TableBatch(kNftDeltable, seq + 7, table.c_str(), NFPROTO_IPV6);
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
  Ack(nft.get(), seq + 8, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, NativeIpv4OutputDnatAlsoRewritesReplyWithoutPostroutingChain) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  Fd client(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd server(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  ASSERT_GE(nft.get(), 0);
  ASSERT_GE(client.get(), 0);
  ASSERT_GE(server.get(), 0);

  sockaddr_in client_address{}, server_address{}, original_address{};
  client_address.sin_family = server_address.sin_family = original_address.sin_family = AF_INET;
  client_address.sin_addr.s_addr = server_address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  original_address.sin_addr.s_addr = inet_addr("127.0.0.2");
  const uint16_t original_port = static_cast<uint16_t>(45000 + getpid() % 10000);
  original_address.sin_port = htons(original_port);
  ASSERT_EQ(0, bind(client.get(), reinterpret_cast<sockaddr*>(&client_address),
                    sizeof(client_address)));
  ASSERT_EQ(0, bind(server.get(), reinterpret_cast<sockaddr*>(&server_address),
                    sizeof(server_address)));
  socklen_t server_len = sizeof(server_address);
  ASSERT_EQ(0, getsockname(server.get(), reinterpret_cast<sockaddr*>(&server_address),
                           &server_len));

  const std::string table = "dkc035_dnat_reply_" + std::to_string(getpid());
  constexpr uint32_t seq = 1430;
  auto batch = TableBatch(kNftNewtable, seq, table.c_str());
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto chain = NewChain(seq + 2, table.c_str(), "output", true, NF_ACCEPT,
                        NF_INET_LOCAL_OUT, NFPROTO_IPV4, -100, "nat");
  auto rule = NativeIpv4DnatRule(seq + 3, table.c_str(), "output",
                                 original_address.sin_addr, original_port,
                                 server_address.sin_addr, ntohs(server_address.sin_port));
  batch.insert(batch.end(), chain.begin(), chain.end());
  batch.insert(batch.end(), rule.begin(), rule.end());
  auto end = Request(NFNL_MSG_BATCH_END, seq + 4);
  batch.insert(batch.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewchain);
  Ack(nft.get(), seq + 3, 0, kNftNewrule);

  const uint8_t request = 0x35, response = 0x36;
  ASSERT_EQ(1, sendto(client.get(), &request, 1, 0,
                      reinterpret_cast<sockaddr*>(&original_address),
                      sizeof(original_address)));
  pollfd ready{server.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 1000));
  sockaddr_in peer{};
  socklen_t peer_len = sizeof(peer);
  uint8_t received = 0;
  ASSERT_EQ(1, recvfrom(server.get(), &received, 1, 0,
                        reinterpret_cast<sockaddr*>(&peer), &peer_len));
  EXPECT_EQ(request, received);
  ASSERT_EQ(1, sendto(server.get(), &response, 1, 0,
                      reinterpret_cast<sockaddr*>(&peer), peer_len));
  ready = {client.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 1000));
  peer_len = sizeof(peer);
  ASSERT_EQ(1, recvfrom(client.get(), &received, 1, 0,
                        reinterpret_cast<sockaddr*>(&peer), &peer_len));
  EXPECT_EQ(response, received);
  EXPECT_EQ(original_address.sin_addr.s_addr, peer.sin_addr.s_addr);
  EXPECT_EQ(original_address.sin_port, peer.sin_port)
      << "the reply must use the original destination even without a POSTROUTING nat chain";

  auto remove = TableBatch(kNftDeltable, seq + 5, table.c_str());
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
  Ack(nft.get(), seq + 6, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, NativeIpv4OutputRedirectRewritesReplyDestination) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  Fd client(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd server(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  ASSERT_GE(nft.get(), 0);
  ASSERT_GE(client.get(), 0);
  ASSERT_GE(server.get(), 0);

  sockaddr_in client_address{}, server_address{}, original_address{};
  client_address.sin_family = server_address.sin_family = original_address.sin_family = AF_INET;
  client_address.sin_addr.s_addr = server_address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  original_address.sin_addr.s_addr = inet_addr("127.0.0.2");
  original_address.sin_port = htons(static_cast<uint16_t>(46000 + getpid() % 10000));
  ASSERT_EQ(0, bind(client.get(), reinterpret_cast<sockaddr*>(&client_address),
                    sizeof(client_address)));
  ASSERT_EQ(0, bind(server.get(), reinterpret_cast<sockaddr*>(&server_address),
                    sizeof(server_address)));
  socklen_t server_len = sizeof(server_address);
  ASSERT_EQ(0, getsockname(server.get(), reinterpret_cast<sockaddr*>(&server_address),
                           &server_len));

  const std::string table = "dkc035_redir_reply_" + std::to_string(getpid());
  constexpr uint32_t seq = 19800;
  auto batch = TableBatch(kNftNewtable, seq, table.c_str());
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto chain = NewChain(seq + 2, table.c_str(), "output", true, NF_ACCEPT,
                        NF_INET_LOCAL_OUT, NFPROTO_IPV4, -100, "nat");
  auto rule = NativeIpv4RedirectRule(seq + 3, table.c_str(), "output",
                                     original_address.sin_addr,
                                     ntohs(original_address.sin_port),
                                     ntohs(server_address.sin_port));
  batch.insert(batch.end(), chain.begin(), chain.end());
  batch.insert(batch.end(), rule.begin(), rule.end());
  auto end = Request(NFNL_MSG_BATCH_END, seq + 4);
  batch.insert(batch.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewchain);
  Ack(nft.get(), seq + 3, 0, kNftNewrule);

  const uint8_t request = 0x37, response = 0x38;
  ASSERT_EQ(1, sendto(client.get(), &request, 1, 0,
                      reinterpret_cast<sockaddr*>(&original_address),
                      sizeof(original_address)));
  pollfd ready{server.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 1000));
  sockaddr_in peer{};
  socklen_t peer_len = sizeof(peer);
  uint8_t received = 0;
  ASSERT_EQ(1, recvfrom(server.get(), &received, 1, 0,
                        reinterpret_cast<sockaddr*>(&peer), &peer_len));
  EXPECT_EQ(request, received);
  ASSERT_EQ(1, sendto(server.get(), &response, 1, 0,
                      reinterpret_cast<sockaddr*>(&peer), peer_len));
  ready = {client.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 1000));
  peer_len = sizeof(peer);
  ASSERT_EQ(1, recvfrom(client.get(), &received, 1, 0,
                        reinterpret_cast<sockaddr*>(&peer), &peer_len));
  EXPECT_EQ(response, received);
  EXPECT_EQ(original_address.sin_addr.s_addr, peer.sin_addr.s_addr);
  EXPECT_EQ(original_address.sin_port, peer.sin_port);

  auto remove = TableBatch(kNftDeltable, seq + 5, table.c_str());
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
  Ack(nft.get(), seq + 6, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, NativeIpv4PreroutingRedirectUsesIngressAddress) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  const unsigned injector_ifindex = if_nametoindex("veth1");
  ASSERT_NE(0u, injector_ifindex) << "requires the network test fixture";
  ASSERT_NE(0u, if_nametoindex("veth2"));
  Fd nft(Open());
  Fd receiver(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd port_reservation(socket(AF_INET, SOCK_DGRAM, 0));
  Fd injector(socket(AF_PACKET, SOCK_DGRAM, htons(ETH_P_IP)));
  ASSERT_GE(nft.get(), 0);
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(port_reservation.get(), 0);
  ASSERT_GE(injector.get(), 0);

  sockaddr_in local{};
  local.sin_family = AF_INET;
  ASSERT_EQ(1, inet_pton(AF_INET, "111.111.11.2", &local.sin_addr));
  ASSERT_EQ(0, bind(receiver.get(), reinterpret_cast<sockaddr*>(&local),
                    sizeof(local)));
  socklen_t local_len = sizeof(local);
  ASSERT_EQ(0, getsockname(receiver.get(), reinterpret_cast<sockaddr*>(&local),
                           &local_len));
  sockaddr_in reserved{};
  reserved.sin_family = AF_INET;
  reserved.sin_addr.s_addr = htonl(INADDR_ANY);
  ASSERT_EQ(0, bind(port_reservation.get(), reinterpret_cast<sockaddr*>(&reserved),
                    sizeof(reserved)));
  socklen_t reserved_len = sizeof(reserved);
  ASSERT_EQ(0, getsockname(port_reservation.get(),
                           reinterpret_cast<sockaddr*>(&reserved), &reserved_len));
  const in_addr original{inet_addr("111.111.11.3")};
  const in_addr remote{inet_addr("111.111.11.99")};

  const std::string table = "dkc035_redir_pre_" + std::to_string(getpid());
  constexpr uint32_t seq = 19900;
  auto batch = TableBatch(kNftNewtable, seq, table.c_str());
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto chain = NewChain(seq + 2, table.c_str(), "prerouting", true, NF_ACCEPT,
                        NF_INET_PRE_ROUTING, NFPROTO_IPV4, -100, "nat");
  auto rule = NativeIpv4RedirectRule(seq + 3, table.c_str(), "prerouting",
                                     original, 12346, ntohs(local.sin_port));
  batch.insert(batch.end(), chain.begin(), chain.end());
  batch.insert(batch.end(), rule.begin(), rule.end());
  auto end = Request(NFNL_MSG_BATCH_END, seq + 4);
  batch.insert(batch.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewchain);
  Ack(nft.get(), seq + 3, 0, kNftNewrule);

  auto packet = Ipv4UdpProbe(remote.s_addr, original.s_addr, 0x350d);
  const uint16_t source_port = ntohs(reserved.sin_port);
  packet[20] = static_cast<uint8_t>(source_port >> 8);
  packet[21] = static_cast<uint8_t>(source_port);
  ASSERT_TRUE(InjectVethIpProbe(injector.get(), injector_ifindex, "veth2", packet))
      << std::strerror(errno);
  pollfd ready{receiver.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 2000));
  sockaddr_in peer{};
  socklen_t peer_len = sizeof(peer);
  uint8_t received = 0;
  ASSERT_EQ(0, recvfrom(receiver.get(), &received, sizeof(received), 0,
                        reinterpret_cast<sockaddr*>(&peer), &peer_len));
  EXPECT_EQ(remote.s_addr, peer.sin_addr.s_addr);
  EXPECT_EQ(htons(source_port), peer.sin_port);

  auto remove = TableBatch(kNftDeltable, seq + 5, table.c_str());
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
  Ack(nft.get(), seq + 6, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, NativeIpv4PostroutingSnatRewritesReplyDestination) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  Fd client(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd server(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  ASSERT_GE(nft.get(), 0);
  ASSERT_GE(client.get(), 0);
  ASSERT_GE(server.get(), 0);

  sockaddr_in client_address{}, server_address{};
  client_address.sin_family = server_address.sin_family = AF_INET;
  client_address.sin_addr.s_addr = inet_addr("127.0.0.2");
  server_address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  ASSERT_EQ(0, bind(client.get(), reinterpret_cast<sockaddr*>(&client_address),
                    sizeof(client_address)));
  ASSERT_EQ(0, bind(server.get(), reinterpret_cast<sockaddr*>(&server_address),
                    sizeof(server_address)));
  socklen_t address_len = sizeof(client_address);
  ASSERT_EQ(0, getsockname(client.get(), reinterpret_cast<sockaddr*>(&client_address),
                           &address_len));
  address_len = sizeof(server_address);
  ASSERT_EQ(0, getsockname(server.get(), reinterpret_cast<sockaddr*>(&server_address),
                           &address_len));

  const std::string table = "dkc035_snat_reply_" + std::to_string(getpid());
  constexpr uint32_t seq = 1450;
  auto batch = TableBatch(kNftNewtable, seq, table.c_str());
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto chain = NewChain(seq + 2, table.c_str(), "postrouting", true, NF_ACCEPT,
                        NF_INET_POST_ROUTING, NFPROTO_IPV4, 100, "nat");
  in_addr translated{};
  translated.s_addr = inet_addr("127.0.0.3");
  auto rule = NativeIpv4SnatRule(seq + 3, table.c_str(), "postrouting",
                                 client_address.sin_addr, translated);
  batch.insert(batch.end(), chain.begin(), chain.end());
  batch.insert(batch.end(), rule.begin(), rule.end());
  auto end = Request(NFNL_MSG_BATCH_END, seq + 4);
  batch.insert(batch.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewchain);
  Ack(nft.get(), seq + 3, 0, kNftNewrule);

  const uint8_t request = 0x37, response = 0x38;
  ASSERT_EQ(1, sendto(client.get(), &request, 1, 0,
                      reinterpret_cast<sockaddr*>(&server_address), sizeof(server_address)));
  pollfd ready{server.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 1000));
  sockaddr_in peer{};
  socklen_t peer_len = sizeof(peer);
  uint8_t received = 0;
  ASSERT_EQ(1, recvfrom(server.get(), &received, 1, 0,
                        reinterpret_cast<sockaddr*>(&peer), &peer_len));
  EXPECT_EQ(request, received);
  EXPECT_EQ(translated.s_addr, peer.sin_addr.s_addr);
  ASSERT_EQ(1, sendto(server.get(), &response, 1, 0,
                      reinterpret_cast<sockaddr*>(&peer), peer_len));
  ready = {client.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 1000));
  peer_len = sizeof(peer);
  ASSERT_EQ(1, recvfrom(client.get(), &received, 1, 0,
                        reinterpret_cast<sockaddr*>(&peer), &peer_len));
  EXPECT_EQ(response, received);
  EXPECT_EQ(server_address.sin_addr.s_addr, peer.sin_addr.s_addr);
  EXPECT_EQ(server_address.sin_port, peer.sin_port);

  auto remove = TableBatch(kNftDeltable, seq + 5, table.c_str());
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
  Ack(nft.get(), seq + 6, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, XtIpv4DnatRevisionZeroRewritesRequestAndReply) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  Fd client(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd server(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  ASSERT_GE(nft.get(), 0);
  ASSERT_GE(client.get(), 0);
  ASSERT_GE(server.get(), 0);

  sockaddr_in client_address{}, server_address{}, original_address{};
  client_address.sin_family = server_address.sin_family = original_address.sin_family = AF_INET;
  client_address.sin_addr.s_addr = server_address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  original_address.sin_addr.s_addr = inet_addr("127.0.0.2");
  original_address.sin_port = htons(static_cast<uint16_t>(45000 + getpid() % 10000));
  ASSERT_EQ(0, bind(client.get(), reinterpret_cast<sockaddr*>(&client_address),
                    sizeof(client_address)));
  ASSERT_EQ(0, bind(server.get(), reinterpret_cast<sockaddr*>(&server_address),
                    sizeof(server_address)));
  socklen_t server_len = sizeof(server_address);
  ASSERT_EQ(0, getsockname(server.get(), reinterpret_cast<sockaddr*>(&server_address),
                           &server_len));

  // xt DNAT is valid only in a table literally named "nat" on Linux.
  const std::string table = "nat";
  constexpr uint32_t seq = 1470;
  auto batch = TableBatch(kNftNewtable, seq, table.c_str());
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto chain = NewChain(seq + 2, table.c_str(), "output", true, NF_ACCEPT,
                        NF_INET_LOCAL_OUT, NFPROTO_IPV4, -100, "nat");
  auto rule = XtIpv4DnatRule(seq + 3, table.c_str(), "output",
                             original_address.sin_addr, ntohs(original_address.sin_port),
                             server_address.sin_addr, ntohs(server_address.sin_port));
  batch.insert(batch.end(), chain.begin(), chain.end());
  batch.insert(batch.end(), rule.begin(), rule.end());
  auto end = Request(NFNL_MSG_BATCH_END, seq + 4);
  batch.insert(batch.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewchain);
  Ack(nft.get(), seq + 3, 0, kNftNewrule);

  const uint8_t request = 0x39, response = 0x3a;
  ASSERT_EQ(1, sendto(client.get(), &request, 1, 0,
                      reinterpret_cast<sockaddr*>(&original_address),
                      sizeof(original_address)));
  pollfd ready{server.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 1000));
  sockaddr_in peer{};
  socklen_t peer_len = sizeof(peer);
  uint8_t received = 0;
  ASSERT_EQ(1, recvfrom(server.get(), &received, 1, 0,
                        reinterpret_cast<sockaddr*>(&peer), &peer_len));
  EXPECT_EQ(request, received);
  ASSERT_EQ(1, sendto(server.get(), &response, 1, 0,
                      reinterpret_cast<sockaddr*>(&peer), peer_len));
  ready = {client.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 1000));
  peer_len = sizeof(peer);
  ASSERT_EQ(1, recvfrom(client.get(), &received, 1, 0,
                        reinterpret_cast<sockaddr*>(&peer), &peer_len));
  EXPECT_EQ(response, received);
  EXPECT_EQ(original_address.sin_addr.s_addr, peer.sin_addr.s_addr);
  EXPECT_EQ(original_address.sin_port, peer.sin_port);

  auto remove = TableBatch(kNftDeltable, seq + 5, table.c_str());
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
  Ack(nft.get(), seq + 6, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, NftIpv6AndInetPreroutingInputReadSameLocalUdp) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  Fd receiver(socket(AF_INET6, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd raw(socket(AF_INET6, SOCK_RAW | SOCK_NONBLOCK, IPPROTO_UDP));
  Fd sender(socket(AF_INET6, SOCK_DGRAM, 0));
  ASSERT_GE(nft.get(), 0);
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(raw.get(), 0);
  ASSERT_GE(sender.get(), 0);
  sockaddr_in6 address{};
  address.sin6_family = AF_INET6;
  address.sin6_addr = in6addr_loopback;
  ASSERT_EQ(0, bind(receiver.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)));
  socklen_t address_len = sizeof(address);
  ASSERT_EQ(0, getsockname(receiver.get(), reinterpret_cast<sockaddr*>(&address), &address_len));

  constexpr uint32_t seq = 1370;
  const std::array<uint8_t, 4> udp_protocol{IPPROTO_UDP, 0, 0, 0};
  std::array<uint8_t, IFNAMSIZ> loopback_name{};
  std::memcpy(loopback_name.data(), "lo", 2);
  uint32_t query_seq = seq + 30;
  for (const auto& [family, table] : {
           std::pair<uint8_t, const char*>{NFPROTO_IPV6, "dkc035_ip6"},
           {NFPROTO_INET, "dkc035_inet"}}) {
    const uint32_t base = seq + (family == NFPROTO_IPV6 ? 0 : 10);
    auto batch = TableBatch(kNftNewtable, base, table, family);
    batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
    auto pre = NewChain(base + 2, table, "pre", true, NF_ACCEPT,
                        NF_INET_PRE_ROUTING, family);
    auto input = NewChain(base + 3, table, "input", true, NF_ACCEPT,
                          NF_INET_LOCAL_IN, family);
    auto pre_iif = MetaCounterRule(base + 4, table, "pre", NFT_META_IIFNAME,
                                   loopback_name.data(), loopback_name.size(), family);
    auto input_proto = MetaCounterRule(base + 5, table, "input", NFT_META_L4PROTO,
                                       udp_protocol.data(), udp_protocol.size(), family);
    for (const auto* message : {&pre, &input, &pre_iif, &input_proto})
      batch.insert(batch.end(), message->begin(), message->end());
    auto end = Request(NFNL_MSG_BATCH_END, base + 6);
    batch.insert(batch.end(), end.begin(), end.end());
    ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
    Ack(nft.get(), base + 1, 0, kNftNewtable);
    Ack(nft.get(), base + 2, 0, kNftNewchain);
    Ack(nft.get(), base + 3, 0, kNftNewchain);
    Ack(nft.get(), base + 4, 0, kNftNewrule);
    Ack(nft.get(), base + 5, 0, kNftNewrule);
  }

  const uint8_t probe = 42;
  ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                      reinterpret_cast<sockaddr*>(&address), address_len));
  pollfd ready{receiver.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 1000));
  uint8_t received = 0;
  ASSERT_EQ(1, recv(receiver.get(), &received, 1, 0));
  EXPECT_EQ(probe, received);
  pollfd raw_ready{raw.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&raw_ready, 1, 1000));
  std::array<uint8_t, 128> raw_packet{};
  ASSERT_GT(recv(raw.get(), raw_packet.data(), raw_packet.size(), 0), 0);

  for (const auto& [family, table] : {
           std::pair<uint8_t, const char*>{NFPROTO_IPV6, "dkc035_ip6"},
           {NFPROTO_INET, "dkc035_inet"}}) {
    for (const char* chain : {"pre", "input"}) {
      auto query = Request(kNftGetrule, query_seq++);
      query[NLMSG_HDRLEN] = family;
      nlmsghdr header{};
      std::memcpy(&header, query.data(), sizeof(header));
      header.nlmsg_flags |= NLM_F_DUMP;
      std::memcpy(query.data(), &header, sizeof(header));
      AppendAttr(&query, NFTA_RULE_TABLE, table);
      AppendAttr(&query, NFTA_RULE_CHAIN, chain);
      ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
      const auto rules = ReceiveRuleDump(nft.get(), header.nlmsg_seq);
      ASSERT_EQ(1u, rules.size());
      EXPECT_TRUE(rules[0].has_counter);
      EXPECT_EQ(1u, rules[0].packets);
    }
    auto remove = TableBatch(kNftDeltable, query_seq, table, family);
    ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
    Ack(nft.get(), query_seq + 1, 0, kNftDeltable);
    query_seq += 3;
  }

  // INPUT must gate both the transport socket and the external raw fanout.
  auto gate = TableBatch(kNftNewtable, query_seq, "dkc035_ip6_gate", NFPROTO_IPV6);
  gate.resize(gate.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto gate_chain = NewChain(query_seq + 2, "dkc035_ip6_gate", "input", true,
                             NF_DROP, NF_INET_LOCAL_IN, NFPROTO_IPV6);
  gate.insert(gate.end(), gate_chain.begin(), gate_chain.end());
  auto gate_end = Request(NFNL_MSG_BATCH_END, query_seq + 3);
  gate.insert(gate.end(), gate_end.begin(), gate_end.end());
  ASSERT_EQ(static_cast<ssize_t>(gate.size()), Send(nft.get(), gate));
  Ack(nft.get(), query_seq + 1, 0, kNftNewtable);
  Ack(nft.get(), query_seq + 2, 0, kNftNewchain);
  const ssize_t sent = sendto(sender.get(), &probe, 1, 0,
                              reinterpret_cast<sockaddr*>(&address), address_len);
  EXPECT_TRUE(sent == 1 || (sent == -1 && errno == EPERM));
  ready.revents = 0;
  raw_ready.revents = 0;
  EXPECT_EQ(0, poll(&ready, 1, 100));
  EXPECT_EQ(0, poll(&raw_ready, 1, 100));
  auto remove_gate = TableBatch(kNftDeltable, query_seq + 4,
                                "dkc035_ip6_gate", NFPROTO_IPV6);
  ASSERT_EQ(static_cast<ssize_t>(remove_gate.size()), Send(nft.get(), remove_gate));
  Ack(nft.get(), query_seq + 5, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, NftInetAndIpv6EqualPriorityUsesNewestBaseChainFirst) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  Fd receiver(socket(AF_INET6, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd raw(socket(AF_INET6, SOCK_RAW | SOCK_NONBLOCK, IPPROTO_UDP));
  Fd sender(socket(AF_INET6, SOCK_DGRAM, 0));
  ASSERT_GE(nft.get(), 0);
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(raw.get(), 0);
  ASSERT_GE(sender.get(), 0);
  sockaddr_in6 address{};
  address.sin6_family = AF_INET6;
  address.sin6_addr = in6addr_loopback;
  ASSERT_EQ(0, bind(receiver.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)));
  socklen_t address_len = sizeof(address);
  ASSERT_EQ(0, getsockname(receiver.get(), reinterpret_cast<sockaddr*>(&address), &address_len));

  constexpr uint32_t seq = 1410;
  auto ip6_batch = TableBatch(kNftNewtable, seq, "dkc035_ip6_order", NFPROTO_IPV6);
  ip6_batch.resize(ip6_batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto ip6_chain = NewChain(seq + 2, "dkc035_ip6_order", "pre", true,
                            NF_ACCEPT, NF_INET_PRE_ROUTING, NFPROTO_IPV6);
  const std::array<uint8_t, 4> udp_protocol{IPPROTO_UDP, 0, 0, 0};
  auto ip6_counter = MetaCounterRule(seq + 3, "dkc035_ip6_order", "pre",
                                     NFT_META_L4PROTO, udp_protocol.data(),
                                     udp_protocol.size(), NFPROTO_IPV6);
  for (const auto* message : {&ip6_chain, &ip6_counter})
    ip6_batch.insert(ip6_batch.end(), message->begin(), message->end());
  auto end = Request(NFNL_MSG_BATCH_END, seq + 4);
  ip6_batch.insert(ip6_batch.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(ip6_batch.size()), Send(nft.get(), ip6_batch));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewchain);
  Ack(nft.get(), seq + 3, 0, kNftNewrule);

  auto inet_batch = TableBatch(kNftNewtable, seq + 5, "dkc035_inet_order", NFPROTO_INET);
  inet_batch.resize(inet_batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto inet_chain = NewChain(seq + 7, "dkc035_inet_order", "pre", true,
                             NF_DROP, NF_INET_PRE_ROUTING, NFPROTO_INET);
  inet_batch.insert(inet_batch.end(), inet_chain.begin(), inet_chain.end());
  end = Request(NFNL_MSG_BATCH_END, seq + 8);
  inet_batch.insert(inet_batch.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(inet_batch.size()), Send(nft.get(), inet_batch));
  Ack(nft.get(), seq + 6, 0, kNftNewtable);
  Ack(nft.get(), seq + 7, 0, kNftNewchain);

  const uint8_t probe = 42;
  const ssize_t sent = sendto(sender.get(), &probe, 1, 0,
                              reinterpret_cast<sockaddr*>(&address), address_len);
  EXPECT_TRUE(sent == 1 || (sent == -1 && errno == EPERM));
  pollfd ready{receiver.get(), POLLIN, 0};
  EXPECT_EQ(0, poll(&ready, 1, 100));
  pollfd raw_ready{raw.get(), POLLIN, 0};
  EXPECT_EQ(0, poll(&raw_ready, 1, 100));

  auto query = Request(kNftGetrule, seq + 9);
  query[NLMSG_HDRLEN] = NFPROTO_IPV6;
  nlmsghdr header{};
  std::memcpy(&header, query.data(), sizeof(header));
  header.nlmsg_flags |= NLM_F_DUMP;
  std::memcpy(query.data(), &header, sizeof(header));
  AppendAttr(&query, NFTA_RULE_TABLE, "dkc035_ip6_order");
  AppendAttr(&query, NFTA_RULE_CHAIN, "pre");
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
  const auto rules = ReceiveRuleDump(nft.get(), seq + 9);
  ASSERT_EQ(1u, rules.size());
  EXPECT_TRUE(rules[0].has_counter);
  EXPECT_EQ(0u, rules[0].packets);

  auto remove_inet = TableBatch(kNftDeltable, seq + 10, "dkc035_inet_order", NFPROTO_INET);
  ASSERT_EQ(static_cast<ssize_t>(remove_inet.size()), Send(nft.get(), remove_inet));
  Ack(nft.get(), seq + 11, 0, kNftDeltable);
  auto remove_ip6 = TableBatch(kNftDeltable, seq + 12, "dkc035_ip6_order", NFPROTO_IPV6);
  ASSERT_EQ(static_cast<ssize_t>(remove_ip6.size()), Send(nft.get(), remove_ip6));
  Ack(nft.get(), seq + 13, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, NftBitwiseBooleanAndShiftsFilterRealPackets) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd receiver(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd sender(socket(AF_INET, SOCK_DGRAM, 0));
  Fd nft(Open());
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(sender.get(), 0);
  ASSERT_GE(nft.get(), 0);
  sockaddr_in address{};
  address.sin_family = AF_INET;
  address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  ASSERT_EQ(0, bind(receiver.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)));
  socklen_t address_len = sizeof(address);
  ASSERT_EQ(0, getsockname(receiver.get(), reinterpret_cast<sockaddr*>(&address), &address_len));

  constexpr uint32_t seq = 1350;
  auto batch = TableBatch(kNftNewtable, seq, "dkc035_bitwise");
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto chain = NewChain(seq + 2, "dkc035_bitwise", "prerouting", true);
  auto rule = ProtocolCounterRule(seq + 3, "dkc035_bitwise", "prerouting",
                                  NFT_CMP_EQ, NFT_PAYLOAD_NETWORK_HEADER,
                                  NFT_REG_1, 9, UINT32_MAX, true);
  auto end = Request(NFNL_MSG_BATCH_END, seq + 4);
  batch.insert(batch.end(), chain.begin(), chain.end());
  batch.insert(batch.end(), rule.begin(), rule.end());
  batch.insert(batch.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewchain);
  Ack(nft.get(), seq + 3, 0, kNftNewrule);

  const uint8_t probe = 35;
  const ssize_t sent = sendto(sender.get(), &probe, 1, 0,
                              reinterpret_cast<sockaddr*>(&address), address_len);
  // Linux may return EPERM synchronously when lo input drops this packet;
  // DragonOS currently injects the local copy through a deferred queue.
  ASSERT_TRUE(sent == 1 || (sent == -1 && errno == EPERM));
  pollfd ready{receiver.get(), POLLIN, 0};
  EXPECT_EQ(0, poll(&ready, 1, 150));

  auto query = Request(kNftGetrule, seq + 5);
  query[NLMSG_HDRLEN] = NFPROTO_IPV4;
  nlmsghdr header{};
  std::memcpy(&header, query.data(), sizeof(header));
  header.nlmsg_flags |= NLM_F_DUMP;
  std::memcpy(query.data(), &header, sizeof(header));
  AppendAttr(&query, NFTA_RULE_TABLE, "dkc035_bitwise");
  AppendAttr(&query, NFTA_RULE_CHAIN, "prerouting");
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
  const auto rules = ReceiveRuleDump(nft.get(), seq + 5);
  ASSERT_EQ(1u, rules.size());
  EXPECT_TRUE(rules[0].has_counter);
  EXPECT_GE(rules[0].packets, 1u);
  EXPECT_EQ((std::vector<uint32_t>{NFT_BITWISE_BOOL, NFT_BITWISE_LSHIFT,
                                   NFT_BITWISE_RSHIFT}), rules[0].bitwise_ops);
  EXPECT_TRUE(rules[0].valid_bitwise_dump);

  auto remove = TableBatch(kNftDeltable, seq + 6, "dkc035_bitwise");
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
  Ack(nft.get(), seq + 7, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, NftBitwisePartiallyOverlappingRegistersFollowLinuxOrder) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd receiver(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd sender(socket(AF_INET, SOCK_DGRAM, 0));
  Fd nft(Open());
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(sender.get(), 0);
  ASSERT_GE(nft.get(), 0);
  sockaddr_in address{};
  address.sin_family = AF_INET;
  address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  ASSERT_EQ(0, bind(receiver.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)));
  socklen_t address_len = sizeof(address);
  ASSERT_EQ(0, getsockname(receiver.get(), reinterpret_cast<sockaddr*>(&address), &address_len));

  constexpr uint32_t seq = 1370;
  auto batch = TableBatch(kNftNewtable, seq, "dkc035_alias");
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto chain = NewChain(seq + 2, "dkc035_alias", "prerouting", true);
  auto rule = AliasedBitwiseRule(seq + 3, "dkc035_alias", "prerouting");
  auto end = Request(NFNL_MSG_BATCH_END, seq + 4);
  batch.insert(batch.end(), chain.begin(), chain.end());
  batch.insert(batch.end(), rule.begin(), rule.end());
  batch.insert(batch.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewchain);
  Ack(nft.get(), seq + 3, 0, kNftNewrule);

  const uint8_t probe = 37;
  const ssize_t sent = sendto(sender.get(), &probe, 1, 0,
                              reinterpret_cast<sockaddr*>(&address), address_len);
  ASSERT_TRUE(sent == 1 || (sent == -1 && errno == EPERM));
  pollfd ready{receiver.get(), POLLIN, 0};
  EXPECT_EQ(0, poll(&ready, 1, 150));
  auto query = Request(kNftGetrule, seq + 5);
  query[NLMSG_HDRLEN] = NFPROTO_IPV4;
  nlmsghdr header{};
  std::memcpy(&header, query.data(), sizeof(header));
  header.nlmsg_flags |= NLM_F_DUMP;
  std::memcpy(query.data(), &header, sizeof(header));
  AppendAttr(&query, NFTA_RULE_TABLE, "dkc035_alias");
  AppendAttr(&query, NFTA_RULE_CHAIN, "prerouting");
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
  const auto rules = ReceiveRuleDump(nft.get(), seq + 5);
  ASSERT_EQ(1u, rules.size());
  EXPECT_GE(rules[0].packets, 1u);

  auto remove = TableBatch(kNftDeltable, seq + 6, "dkc035_alias");
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
  Ack(nft.get(), seq + 7, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, NftBitwiseOpAbovePolicyLimitRollsBackBatch) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  ASSERT_GE(nft.get(), 0);
  constexpr uint32_t seq = 1380;
  auto batch = TableBatch(kNftNewtable, seq, "dkc035_badop");
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto chain = NewChain(seq + 2, "dkc035_badop", "regular");
  auto rule = AliasedBitwiseRule(seq + 3, "dkc035_badop", "regular", 256);
  auto end = Request(NFNL_MSG_BATCH_END, seq + 4);
  batch.insert(batch.end(), chain.begin(), chain.end());
  batch.insert(batch.end(), rule.begin(), rule.end());
  batch.insert(batch.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewchain);
  Ack(nft.get(), seq + 3, ERANGE, kNftNewrule);
  auto query = Request(kNftGettable, seq + 5);
  query[NLMSG_HDRLEN] = NFPROTO_IPV4;
  AppendAttr(&query, NFTA_TABLE_NAME, "dkc035_badop");
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
  Ack(nft.get(), seq + 5, ENOENT, kNftGettable);
}

TEST(NetlinkNetfilter, NftBitwiseInvalidShiftRollsBackBatch) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  ASSERT_GE(nft.get(), 0);
  constexpr uint32_t seq = 1360;
  auto batch = TableBatch(kNftNewtable, seq, "dkc035_badshift");
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto chain = NewChain(seq + 2, "dkc035_badshift", "regular");
  auto rule = ProtocolCounterRule(seq + 3, "dkc035_badshift", "regular",
                                  NFT_CMP_EQ, NFT_PAYLOAD_NETWORK_HEADER,
                                  NFT_REG_1, 9, UINT32_MAX, true, 32);
  auto end = Request(NFNL_MSG_BATCH_END, seq + 4);
  batch.insert(batch.end(), chain.begin(), chain.end());
  batch.insert(batch.end(), rule.begin(), rule.end());
  batch.insert(batch.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewchain);
  Ack(nft.get(), seq + 3, EINVAL, kNftNewrule);

  auto query = Request(kNftGettable, seq + 5);
  query[NLMSG_HDRLEN] = NFPROTO_IPV4;
  AppendAttr(&query, NFTA_TABLE_NAME, "dkc035_badshift");
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
  Ack(nft.get(), seq + 5, ENOENT, kNftGettable);
}

TEST(NetlinkNetfilter, NftPayloadBoundsAndRegisterFlowRejectAtomically) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  ASSERT_GE(nft.get(), 0);
  auto rejected = [&](const char* table_name, uint32_t seq,
                      uint32_t offset, uint32_t compare_register,
                      int expected_error) {
    auto batch = TableBatch(kNftNewtable, seq, table_name);
    batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
    auto chain = NewChain(seq + 2, table_name, "regular");
    auto rule = ProtocolCounterRule(seq + 3, table_name, "regular", NFT_CMP_EQ,
                                    NFT_PAYLOAD_NETWORK_HEADER, NFT_REG_1,
                                    offset, compare_register);
    auto end = Request(NFNL_MSG_BATCH_END, seq + 4);
    batch.insert(batch.end(), chain.begin(), chain.end());
    batch.insert(batch.end(), rule.begin(), rule.end());
    batch.insert(batch.end(), end.begin(), end.end());
    ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
    Ack(nft.get(), seq + 1, 0, kNftNewtable);
    Ack(nft.get(), seq + 2, 0, kNftNewchain);
    Ack(nft.get(), seq + 3, expected_error, kNftNewrule);
    auto query = Request(kNftGettable, seq + 5);
    query[NLMSG_HDRLEN] = NFPROTO_IPV4;
    AppendAttr(&query, NFTA_TABLE_NAME, table_name);
    ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
    Ack(nft.get(), seq + 5, ENOENT, kNftGettable);
  };
  rejected("dkc035_offset", 1210, 256, NFT_REG_1, ERANGE);
  rejected("dkc035_register", 1220, 9, NFT_REG_2, ENODATA);
}

TEST(NetlinkNetfilter, NftShortPacketBreaksRuleWithoutDropping) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd receiver(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd sender(socket(AF_INET, SOCK_DGRAM, 0));
  Fd nft(Open());
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(sender.get(), 0);
  ASSERT_GE(nft.get(), 0);
  sockaddr_in address{};
  address.sin_family = AF_INET;
  address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  ASSERT_EQ(0, bind(receiver.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)));
  socklen_t address_len = sizeof(address);
  ASSERT_EQ(0, getsockname(receiver.get(), reinterpret_cast<sockaddr*>(&address), &address_len));

  constexpr uint32_t seq = 1230;
  auto batch = TableBatch(kNftNewtable, seq, "dkc035_break");
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto chain = NewChain(seq + 2, "dkc035_break", "prerouting", true);
  auto short_read = ProtocolCounterRule(seq + 3, "dkc035_break", "prerouting",
                                        NFT_CMP_EQ, NFT_PAYLOAD_NETWORK_HEADER,
                                        NFT_REG_1, 255);
  auto end = Request(NFNL_MSG_BATCH_END, seq + 4);
  batch.insert(batch.end(), chain.begin(), chain.end());
  batch.insert(batch.end(), short_read.begin(), short_read.end());
  batch.insert(batch.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewchain);
  Ack(nft.get(), seq + 3, 0, kNftNewrule);

  const uint8_t probe = 23;
  ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                      reinterpret_cast<sockaddr*>(&address), address_len));
  pollfd ready{receiver.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 2000));
  uint8_t received = 0;
  ASSERT_EQ(1, recv(receiver.get(), &received, 1, 0));
  EXPECT_EQ(probe, received);

  auto query = Request(kNftGetrule, seq + 5);
  query[NLMSG_HDRLEN] = NFPROTO_IPV4;
  nlmsghdr header{};
  std::memcpy(&header, query.data(), sizeof(header));
  header.nlmsg_flags |= NLM_F_DUMP;
  std::memcpy(query.data(), &header, sizeof(header));
  AppendAttr(&query, NFTA_RULE_TABLE, "dkc035_break");
  AppendAttr(&query, NFTA_RULE_CHAIN, "prerouting");
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
  const auto rules = ReceiveRuleDump(nft.get(), seq + 5);
  ASSERT_EQ(1u, rules.size());
  EXPECT_TRUE(rules[0].has_counter);
  EXPECT_EQ(0u, rules[0].packets);

  auto remove = TableBatch(kNftDeltable, seq + 6, "dkc035_break");
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
  Ack(nft.get(), seq + 7, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, NftPreroutingPolicyDropsLoopbackUdp) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd receiver(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd sender(socket(AF_INET, SOCK_DGRAM, 0));
  Fd nft(Open());
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(sender.get(), 0);
  ASSERT_GE(nft.get(), 0);
  sockaddr_in address{};
  address.sin_family = AF_INET;
  address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  ASSERT_EQ(0, bind(receiver.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)));
  socklen_t address_len = sizeof(address);
  ASSERT_EQ(0, getsockname(receiver.get(), reinterpret_cast<sockaddr*>(&address), &address_len));
  const uint8_t probe = 7;
  ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                      reinterpret_cast<sockaddr*>(&address), address_len));
  pollfd ready{receiver.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 2000));
  uint8_t received = 0;
  ASSERT_EQ(1, recv(receiver.get(), &received, 1, 0));

  constexpr uint32_t seq = 900;
  auto begin = Request(NFNL_MSG_BATCH_BEGIN, seq);
  nfgenmsg begin_gen{};
  begin_gen.res_id = htons(NFNL_SUBSYS_NFTABLES);
  std::memcpy(begin.data() + NLMSG_HDRLEN, &begin_gen, sizeof(begin_gen));
  auto table = Request(kNftNewtable, seq + 1);
  table[NLMSG_HDRLEN] = NFPROTO_IPV4;
  nlmsghdr header{};
  std::memcpy(&header, table.data(), sizeof(header));
  header.nlmsg_flags |= NLM_F_CREATE | NLM_F_EXCL;
  std::memcpy(table.data(), &header, sizeof(header));
  AppendAttr(&table, NFTA_TABLE_NAME, "dkc035_gate");

  auto chain = Request(kNftNewchain, seq + 2);
  chain[NLMSG_HDRLEN] = NFPROTO_IPV4;
  std::memcpy(&header, chain.data(), sizeof(header));
  header.nlmsg_flags |= NLM_F_CREATE | NLM_F_EXCL;
  std::memcpy(chain.data(), &header, sizeof(header));
  AppendAttr(&chain, NFTA_CHAIN_TABLE, "dkc035_gate");
  AppendAttr(&chain, NFTA_CHAIN_NAME, "prerouting");
  AppendAttr(&chain, NFTA_CHAIN_TYPE, "filter");
  std::vector<uint8_t> hook;
  const uint32_t hooknum = htonl(NF_INET_PRE_ROUTING);
  const uint32_t priority = htonl(0);
  AppendNla(&hook, NFTA_HOOK_HOOKNUM,
            reinterpret_cast<const uint8_t*>(&hooknum), sizeof(hooknum));
  AppendNla(&hook, NFTA_HOOK_PRIORITY,
            reinterpret_cast<const uint8_t*>(&priority), sizeof(priority));
  AppendRawAttr(&chain, NFTA_CHAIN_HOOK | NLA_F_NESTED, hook.data(), hook.size());
  AppendBe32(&chain, NFTA_CHAIN_POLICY, NF_DROP);

  auto end = Request(NFNL_MSG_BATCH_END, seq + 3);
  begin.insert(begin.end(), table.begin(), table.end());
  begin.insert(begin.end(), chain.begin(), chain.end());
  begin.insert(begin.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(begin.size()), Send(nft.get(), begin));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewchain);

  ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                      reinterpret_cast<sockaddr*>(&address), address_len));
  ready.revents = 0;
  EXPECT_EQ(0, poll(&ready, 1, 150));

  auto remove = TableBatch(kNftDeltable, seq + 4, "dkc035_gate");
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
  Ack(nft.get(), seq + 5, 0, kNftDeltable);
}

// Raw loopback must enter the same IP receive path as ordinary loopback UDP.
TEST(NetlinkNetfilter, NftPreroutingDropBlocksRawLoopback) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd receiver(socket(AF_INET, SOCK_RAW | SOCK_NONBLOCK, IPPROTO_UDP));
  Fd sender(socket(AF_INET, SOCK_RAW, IPPROTO_UDP));
  Fd nft(Open());
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(sender.get(), 0);
  ASSERT_GE(nft.get(), 0);

  sockaddr_in address{};
  address.sin_family = AF_INET;
  address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  std::array<uint8_t, 8> udp_header{};
  udp_header[1] = 20;
  udp_header[3] = 21;
  udp_header[5] = 8;
  ASSERT_EQ(static_cast<ssize_t>(udp_header.size()),
            sendto(sender.get(), udp_header.data(), udp_header.size(), 0,
                   reinterpret_cast<sockaddr*>(&address), sizeof(address)));
  pollfd ready{receiver.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 2000));
  std::array<uint8_t, 128> received{};
  ASSERT_GT(recv(receiver.get(), received.data(), received.size(), 0), 0);

  constexpr uint32_t seq = 920;
  auto batch = TableBatch(kNftNewtable, seq, "dkc035_raw_gate");
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto chain = NewChain(seq + 2, "dkc035_raw_gate", "prerouting", true, NF_DROP);
  batch.insert(batch.end(), chain.begin(), chain.end());
  auto end = Request(NFNL_MSG_BATCH_END, seq + 3);
  batch.insert(batch.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewchain);

  const ssize_t sent = sendto(sender.get(), udp_header.data(), udp_header.size(), 0,
                              reinterpret_cast<sockaddr*>(&address), sizeof(address));
  EXPECT_TRUE(sent == static_cast<ssize_t>(udp_header.size()) ||
              (sent == -1 && errno == EPERM));
  ready.revents = 0;
  EXPECT_EQ(0, poll(&ready, 1, 150));

  auto remove = TableBatch(kNftDeltable, seq + 4, "dkc035_raw_gate");
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
  Ack(nft.get(), seq + 5, 0, kNftDeltable);
}

// LOCAL_IN must run after PRE_ROUTING and before protocol socket fanout.
// Otherwise UDP and raw listeners may observe a packet that a local filter
// accepted for installation but should have dropped.
TEST(NetlinkNetfilter, NftLocalInputDropBlocksUdpAndRawLoopback) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd udp(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd raw(socket(AF_INET, SOCK_RAW | SOCK_NONBLOCK, IPPROTO_UDP));
  Fd sender(socket(AF_INET, SOCK_DGRAM, 0));
  Fd nft(Open());
  ASSERT_GE(udp.get(), 0);
  ASSERT_GE(raw.get(), 0);
  ASSERT_GE(sender.get(), 0);
  ASSERT_GE(nft.get(), 0);

  sockaddr_in address{};
  address.sin_family = AF_INET;
  address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  ASSERT_EQ(0, bind(udp.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)));
  socklen_t address_len = sizeof(address);
  ASSERT_EQ(0, getsockname(udp.get(), reinterpret_cast<sockaddr*>(&address), &address_len));
  const uint8_t probe = 42;
  ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                      reinterpret_cast<sockaddr*>(&address), address_len));
  pollfd ready[2]{{udp.get(), POLLIN, 0}, {raw.get(), POLLIN, 0}};
  ASSERT_EQ(1, poll(&ready[0], 1, 2000));
  ASSERT_EQ(1, poll(&ready[1], 1, 2000));
  std::array<uint8_t, 128> buffer{};
  ASSERT_EQ(1, recv(udp.get(), buffer.data(), buffer.size(), 0));
  ASSERT_GT(recv(raw.get(), buffer.data(), buffer.size(), 0), 0);

  constexpr uint32_t seq = 922;
  auto batch = TableBatch(kNftNewtable, seq, "dkc035_local_in");
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto chain = NewChain(seq + 2, "dkc035_local_in", "input", true, NF_DROP,
                        NF_INET_LOCAL_IN);
  batch.insert(batch.end(), chain.begin(), chain.end());
  auto end = Request(NFNL_MSG_BATCH_END, seq + 3);
  batch.insert(batch.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewchain);

  const ssize_t sent = sendto(sender.get(), &probe, 1, 0,
                              reinterpret_cast<sockaddr*>(&address), address_len);
  EXPECT_TRUE(sent == 1 || (sent == -1 && errno == EPERM));
  EXPECT_EQ(0, poll(ready, 2, 150));

  auto remove = TableBatch(kNftDeltable, seq + 4, "dkc035_local_in");
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
  Ack(nft.get(), seq + 5, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, NativeRuleAcceptsIptablesCompatMetadata) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  ASSERT_GE(nft.get(), 0);
  const std::string table = "dkc035_ipt_" + std::to_string(getpid());
  constexpr uint32_t seq = 924;
  bool installed = false;
  [&] {
    std::vector<uint8_t> expressions;
    AppendExpr(&expressions, "counter", {});
    std::vector<uint8_t> verdict;
    const uint32_t drop = htonl(NF_DROP);
    AppendNla(&verdict, NFTA_VERDICT_CODE,
              reinterpret_cast<const uint8_t*>(&drop), sizeof(drop));
    std::vector<uint8_t> data;
    AppendNla(&data, NFTA_DATA_VERDICT | NLA_F_NESTED,
              verdict.data(), verdict.size());
    std::vector<uint8_t> immediate;
    const uint32_t register_id = htonl(NFT_REG_VERDICT);
    AppendNla(&immediate, NFTA_IMMEDIATE_DREG,
              reinterpret_cast<const uint8_t*>(&register_id), sizeof(register_id));
    AppendNla(&immediate, NFTA_IMMEDIATE_DATA | NLA_F_NESTED,
              data.data(), data.size());
    AppendExpr(&expressions, "immediate", immediate);
    auto rule = RuleWithExpressions(seq + 3, table.c_str(), "input", expressions);
    std::vector<uint8_t> compat;
    const uint32_t zero = 0;
    AppendNla(&compat, NFTA_RULE_COMPAT_PROTO,
              reinterpret_cast<const uint8_t*>(&zero), sizeof(zero));
    AppendNla(&compat, NFTA_RULE_COMPAT_FLAGS,
              reinterpret_cast<const uint8_t*>(&zero), sizeof(zero));
    AppendRawAttr(&rule, NFTA_RULE_COMPAT | NLA_F_NESTED,
                  compat.data(), compat.size());

    // Linux only interprets compat's children for xt match/target expressions.
    // A native-only rule with empty compat metadata is equally valid.
    auto empty_compat_rule = RuleWithExpressions(seq + 4, table.c_str(),
                                                 "input", expressions);
    AppendRawAttr(&empty_compat_rule, NFTA_RULE_COMPAT | NLA_F_NESTED,
                  nullptr, 0);

    auto batch = TableBatch(kNftNewtable, seq, table.c_str());
    batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
    auto chain = NewChain(seq + 2, table.c_str(), "input", true, NF_ACCEPT,
                          NF_INET_LOCAL_IN);
    batch.insert(batch.end(), chain.begin(), chain.end());
    batch.insert(batch.end(), rule.begin(), rule.end());
    batch.insert(batch.end(), empty_compat_rule.begin(), empty_compat_rule.end());
    auto end = Request(NFNL_MSG_BATCH_END, seq + 5);
    batch.insert(batch.end(), end.begin(), end.end());
    ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
    installed = true;
    Ack(nft.get(), seq + 1, 0, kNftNewtable);
    Ack(nft.get(), seq + 2, 0, kNftNewchain);
    Ack(nft.get(), seq + 3, 0, kNftNewrule);
    Ack(nft.get(), seq + 4, 0, kNftNewrule);

    Fd receiver(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
    Fd sender(socket(AF_INET, SOCK_DGRAM, 0));
    ASSERT_GE(receiver.get(), 0);
    ASSERT_GE(sender.get(), 0);
    sockaddr_in address{};
    address.sin_family = AF_INET;
    address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    ASSERT_EQ(0, bind(receiver.get(), reinterpret_cast<sockaddr*>(&address),
                      sizeof(address)));
    socklen_t length = sizeof(address);
    ASSERT_EQ(0, getsockname(receiver.get(), reinterpret_cast<sockaddr*>(&address),
                             &length));
    const uint8_t probe = 0x35;
    const ssize_t sent = sendto(sender.get(), &probe, sizeof(probe), 0,
                                reinterpret_cast<sockaddr*>(&address), length);
    EXPECT_TRUE(sent == 1 || (sent == -1 && errno == EPERM));
    pollfd ready{receiver.get(), POLLIN, 0};
    EXPECT_EQ(0, poll(&ready, 1, 150));
  }();
  if (installed) {
    auto remove = TableBatch(kNftDeltable, seq + 6, table.c_str());
    ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
    Ack(nft.get(), seq + 7, 0, kNftDeltable);
  }
}

TEST(NetlinkNetfilter, NftCompatTcpMatchRevisionQuery) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  ASSERT_GE(nft.get(), 0);
  constexpr uint32_t seq = 928;
  auto query = Request(kNftCompatGet, seq);
  query[NLMSG_HDRLEN] = NFPROTO_IPV4;
  nlmsghdr header{};
  std::memcpy(&header, query.data(), sizeof(header));
  header.nlmsg_flags &= ~NLM_F_ACK;
  std::memcpy(query.data(), &header, sizeof(header));
  AppendAttr(&query, NFTA_COMPAT_NAME, "tcp");
  AppendBe32(&query, NFTA_COMPAT_REV, 0);
  AppendBe32(&query, NFTA_COMPAT_TYPE, 0);  // Match, not target.
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
  pollfd ready{nft.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 2000));
  std::array<uint8_t, 512> bytes{};
  const ssize_t count = recv(nft.get(), bytes.data(), bytes.size(), 0);
  ASSERT_GE(count, static_cast<ssize_t>(NLMSG_LENGTH(sizeof(nfgenmsg))));
  nlmsghdr reply{};
  std::memcpy(&reply, bytes.data(), sizeof(reply));
  ASSERT_EQ(kNftCompatGet, reply.nlmsg_type);
  EXPECT_EQ(seq, reply.nlmsg_seq);
  ASSERT_LE(reply.nlmsg_len, static_cast<uint32_t>(count));
  const auto attrs = NlaSpan{bytes.data() + NLMSG_HDRLEN + sizeof(nfgenmsg),
                             reply.nlmsg_len - NLMSG_HDRLEN - sizeof(nfgenmsg)};
  const auto name = FindNla(attrs, NFTA_COMPAT_NAME);
  const auto revision = FindNla(attrs, NFTA_COMPAT_REV);
  const auto type = FindNla(attrs, NFTA_COMPAT_TYPE);
  ASSERT_EQ(sizeof("tcp"), name.length);
  EXPECT_EQ(0, std::memcmp(name.data, "tcp", name.length));
  EXPECT_EQ(0u, Be32(revision));
  EXPECT_EQ(0u, Be32(type));

  auto missing_type = Request(kNftCompatGet, seq + 1);
  missing_type[NLMSG_HDRLEN] = NFPROTO_IPV4;
  AppendAttr(&missing_type, NFTA_COMPAT_NAME, "tcp");
  AppendBe32(&missing_type, NFTA_COMPAT_REV, 0);
  ASSERT_EQ(static_cast<ssize_t>(missing_type.size()), Send(nft.get(), missing_type));
  Ack(nft.get(), seq + 1, EINVAL, kNftCompatGet);

  auto wrong_family = Request(kNftCompatGet, seq + 2);
  wrong_family[NLMSG_HDRLEN] = NFPROTO_UNSPEC;
  AppendAttr(&wrong_family, NFTA_COMPAT_NAME, "tcp");
  AppendBe32(&wrong_family, NFTA_COMPAT_REV, 0);
  AppendBe32(&wrong_family, NFTA_COMPAT_TYPE, 0);
  ASSERT_EQ(static_cast<ssize_t>(wrong_family.size()), Send(nft.get(), wrong_family));
  Ack(nft.get(), seq + 2, EINVAL, kNftCompatGet);
}

TEST(NetlinkNetfilter, NftCompatAddrtypeLocalUsesFibAndDumps) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  Fd receiver(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd sender(socket(AF_INET, SOCK_DGRAM, 0));
  ASSERT_GE(nft.get(), 0);
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(sender.get(), 0);

  sockaddr_in address{};
  address.sin_family = AF_INET;
  address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  ASSERT_EQ(0, bind(receiver.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)));
  socklen_t length = sizeof(address);
  ASSERT_EQ(0, getsockname(receiver.get(), reinterpret_cast<sockaddr*>(&address), &length));
  const uint8_t probe = 0x35;
  auto send_probe = [&] {
    return sendto(sender.get(), &probe, sizeof(probe), 0,
                  reinterpret_cast<const sockaddr*>(&address), length);
  };
  pollfd ready{receiver.get(), POLLIN, 0};
  ASSERT_EQ(1, send_probe());
  ASSERT_EQ(1, poll(&ready, 1, 2000));
  uint8_t received = 0;
  ASSERT_EQ(1, recv(receiver.get(), &received, sizeof(received), 0));

  const std::string table = "dkc035_addrtype_" + std::to_string(getpid());
  constexpr uint32_t seq = 946;
  auto query = Request(kNftCompatGet, seq);
  query[NLMSG_HDRLEN] = NFPROTO_IPV4;
  nlmsghdr query_header{};
  std::memcpy(&query_header, query.data(), sizeof(query_header));
  query_header.nlmsg_flags &= ~NLM_F_ACK;
  std::memcpy(query.data(), &query_header, sizeof(query_header));
  AppendAttr(&query, NFTA_COMPAT_NAME, "addrtype");
  AppendBe32(&query, NFTA_COMPAT_REV, 1);
  AppendBe32(&query, NFTA_COMPAT_TYPE, 0);
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
  pollfd reply_ready{nft.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&reply_ready, 1, 2000));
  std::array<uint8_t, 512> reply_bytes{};
  const ssize_t reply_len = recv(nft.get(), reply_bytes.data(), reply_bytes.size(), 0);
  ASSERT_GE(reply_len, static_cast<ssize_t>(NLMSG_LENGTH(sizeof(nfgenmsg))));
  nlmsghdr reply{};
  std::memcpy(&reply, reply_bytes.data(), sizeof(reply));
  ASSERT_EQ(kNftCompatGet, reply.nlmsg_type);
  const auto query_attrs = NlaSpan{reply_bytes.data() + NLMSG_HDRLEN + sizeof(nfgenmsg),
                                   reply.nlmsg_len - NLMSG_HDRLEN - sizeof(nfgenmsg)};
  EXPECT_EQ(1u, Be32(FindNla(query_attrs, NFTA_COMPAT_REV)));

  xt_addrtype_info_v1 addrtype{};
  addrtype.dest = XT_ADDRTYPE_LOCAL;
  std::vector<uint8_t> match;
  AppendNla(&match, NFTA_MATCH_NAME,
            reinterpret_cast<const uint8_t*>("addrtype"), sizeof("addrtype"));
  const uint32_t revision = htonl(1);
  AppendNla(&match, NFTA_MATCH_REV,
            reinterpret_cast<const uint8_t*>(&revision), sizeof(revision));
  AppendNla(&match, NFTA_MATCH_INFO,
            reinterpret_cast<const uint8_t*>(&addrtype), sizeof(addrtype));
  std::vector<uint8_t> expressions;
  AppendExpr(&expressions, "match", match);
  AppendExpr(&expressions, "counter", {});
  std::vector<uint8_t> verdict;
  const uint32_t drop = htonl(NF_DROP);
  AppendNla(&verdict, NFTA_VERDICT_CODE,
            reinterpret_cast<const uint8_t*>(&drop), sizeof(drop));
  std::vector<uint8_t> data;
  AppendNla(&data, NFTA_DATA_VERDICT | NLA_F_NESTED, verdict.data(), verdict.size());
  std::vector<uint8_t> immediate;
  const uint32_t verdict_reg = htonl(NFT_REG_VERDICT);
  AppendNla(&immediate, NFTA_IMMEDIATE_DREG,
            reinterpret_cast<const uint8_t*>(&verdict_reg), sizeof(verdict_reg));
  AppendNla(&immediate, NFTA_IMMEDIATE_DATA | NLA_F_NESTED, data.data(), data.size());
  AppendExpr(&expressions, "immediate", immediate);
  auto rule = RuleWithExpressions(seq + 4, table.c_str(), "input", expressions);
  std::vector<uint8_t> compat;
  const uint32_t zero = 0;
  AppendNla(&compat, NFTA_RULE_COMPAT_PROTO,
            reinterpret_cast<const uint8_t*>(&zero), sizeof(zero));
  AppendNla(&compat, NFTA_RULE_COMPAT_FLAGS,
            reinterpret_cast<const uint8_t*>(&zero), sizeof(zero));
  AppendRawAttr(&rule, NFTA_RULE_COMPAT | NLA_F_NESTED, compat.data(), compat.size());

  auto batch = TableBatch(kNftNewtable, seq + 1, table.c_str());
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto chain = NewChain(seq + 3, table.c_str(), "input", true, NF_ACCEPT,
                        NF_INET_LOCAL_IN);
  auto end = Request(NFNL_MSG_BATCH_END, seq + 5);
  batch.insert(batch.end(), chain.begin(), chain.end());
  batch.insert(batch.end(), rule.begin(), rule.end());
  batch.insert(batch.end(), end.begin(), end.end());
  bool installed = false;
  [&] {
    ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
    installed = true;
    Ack(nft.get(), seq + 2, 0, kNftNewtable);
    Ack(nft.get(), seq + 3, 0, kNftNewchain);
    Ack(nft.get(), seq + 4, 0, kNftNewrule);
    const ssize_t sent = send_probe();
    EXPECT_TRUE(sent == 1 || (sent == -1 && errno == EPERM));
    ready.revents = 0;
    EXPECT_EQ(0, poll(&ready, 1, 150));
    auto dump = Request(kNftGetrule, seq + 8);
    dump[NLMSG_HDRLEN] = NFPROTO_IPV4;
    nlmsghdr dump_header{};
    std::memcpy(&dump_header, dump.data(), sizeof(dump_header));
    dump_header.nlmsg_flags |= NLM_F_DUMP;
    std::memcpy(dump.data(), &dump_header, sizeof(dump_header));
    AppendAttr(&dump, NFTA_RULE_TABLE, table.c_str());
    AppendAttr(&dump, NFTA_RULE_CHAIN, "input");
    ASSERT_EQ(static_cast<ssize_t>(dump.size()), Send(nft.get(), dump));
    const auto rules = ReceiveRuleDump(nft.get(), seq + 8);
    ASSERT_EQ(1u, rules.size());
    EXPECT_TRUE(rules[0].has_counter);
    EXPECT_GE(rules[0].packets, 1u);
  }();
  if (installed) {
    auto remove = TableBatch(kNftDeltable, seq + 6, table.c_str());
    ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
    Ack(nft.get(), seq + 7, 0, kNftDeltable);
    ASSERT_EQ(1, send_probe());
    ready.revents = 0;
    ASSERT_EQ(1, poll(&ready, 1, 2000));
    EXPECT_EQ(1, recv(receiver.get(), &received, sizeof(received), 0));
  }
}

TEST(NetlinkNetfilter, NftIpv6AddrtypeLocalMatchesRealLoopbackIngress) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  Fd receiver(socket(AF_INET6, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd sender(socket(AF_INET6, SOCK_DGRAM, 0));
  ASSERT_GE(nft.get(), 0);
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(sender.get(), 0);
  sockaddr_in6 address{};
  address.sin6_family = AF_INET6;
  address.sin6_addr = in6addr_loopback;
  ASSERT_EQ(0, bind(receiver.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)));
  socklen_t address_len = sizeof(address);
  ASSERT_EQ(0, getsockname(receiver.get(), reinterpret_cast<sockaddr*>(&address), &address_len));
  const uint8_t probe = 0x36;
  auto send_probe = [&] {
    return sendto(sender.get(), &probe, 1, 0,
                  reinterpret_cast<const sockaddr*>(&address), address_len);
  };
  ASSERT_EQ(1, send_probe());
  pollfd ready{receiver.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 1000));
  uint8_t received = 0;
  ASSERT_EQ(1, recv(receiver.get(), &received, 1, 0));

  const std::string table = "dkc035_addrtype6_" + std::to_string(getpid());
  constexpr uint32_t seq = 1490;
  auto batch = TableBatch(kNftNewtable, seq, table.c_str(), NFPROTO_IPV6);
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto chain = NewChain(seq + 2, table.c_str(), "prerouting", true, NF_ACCEPT,
                        NF_INET_PRE_ROUTING, NFPROTO_IPV6);
  auto rule = Ipv6AddrtypeLocalDropRule(seq + 3, table.c_str(), "prerouting");
  batch.insert(batch.end(), chain.begin(), chain.end());
  batch.insert(batch.end(), rule.begin(), rule.end());
  auto end = Request(NFNL_MSG_BATCH_END, seq + 4);
  batch.insert(batch.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewchain);
  Ack(nft.get(), seq + 3, 0, kNftNewrule);

  const ssize_t sent = send_probe();
  EXPECT_TRUE(sent == 1 || (sent == -1 && errno == EPERM));
  ready.revents = 0;
  EXPECT_EQ(0, poll(&ready, 1, 150));
  auto query = Request(kNftGetrule, seq + 5);
  query[NLMSG_HDRLEN] = NFPROTO_IPV6;
  nlmsghdr header{};
  std::memcpy(&header, query.data(), sizeof(header));
  header.nlmsg_flags |= NLM_F_DUMP;
  std::memcpy(query.data(), &header, sizeof(header));
  AppendAttr(&query, NFTA_RULE_TABLE, table.c_str());
  AppendAttr(&query, NFTA_RULE_CHAIN, "prerouting");
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
  const auto rules = ReceiveRuleDump(nft.get(), seq + 5);
  ASSERT_EQ(1u, rules.size());
  EXPECT_TRUE(rules[0].has_counter);
  EXPECT_GE(rules[0].packets, 1u);

  auto remove = TableBatch(kNftDeltable, seq + 6, table.c_str(), NFPROTO_IPV6);
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
  Ack(nft.get(), seq + 7, 0, kNftDeltable);
  ASSERT_EQ(1, send_probe());
  ready.revents = 0;
  ASSERT_EQ(1, poll(&ready, 1, 1000));
  EXPECT_EQ(1, recv(receiver.get(), &received, 1, 0));
}

TEST(NetlinkNetfilter, NftIpv6AddrtypeLocalDropsAtOutputAndPostrouting) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  Fd receiver(socket(AF_INET6, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd sender(socket(AF_INET6, SOCK_DGRAM, 0));
  ASSERT_GE(nft.get(), 0);
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(sender.get(), 0);
  sockaddr_in6 address{};
  address.sin6_family = AF_INET6;
  address.sin6_addr = in6addr_loopback;
  ASSERT_EQ(0, bind(receiver.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)));
  socklen_t address_len = sizeof(address);
  ASSERT_EQ(0, getsockname(receiver.get(), reinterpret_cast<sockaddr*>(&address), &address_len));
  const uint8_t probe = 0x37;
  auto send_probe = [&] {
    return sendto(sender.get(), &probe, 1, 0,
                  reinterpret_cast<const sockaddr*>(&address), address_len);
  };
  for (const auto& [hook, name] : {
           std::pair<uint32_t, const char*>{NF_INET_LOCAL_OUT, "output"},
           {NF_INET_POST_ROUTING, "postrouting"}}) {
    const std::string table = std::string("dkc035_addrtype6_") + name +
                              "_" + std::to_string(getpid());
    const uint32_t seq = 1510 + hook * 10;
    auto batch = TableBatch(kNftNewtable, seq, table.c_str(), NFPROTO_IPV6);
    batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
    auto chain = NewChain(seq + 2, table.c_str(), name, true, NF_ACCEPT,
                          hook, NFPROTO_IPV6);
    auto rule = Ipv6AddrtypeLocalDropRule(seq + 3, table.c_str(), name);
    batch.insert(batch.end(), chain.begin(), chain.end());
    batch.insert(batch.end(), rule.begin(), rule.end());
    auto end = Request(NFNL_MSG_BATCH_END, seq + 4);
    batch.insert(batch.end(), end.begin(), end.end());
    ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
    Ack(nft.get(), seq + 1, 0, kNftNewtable);
    Ack(nft.get(), seq + 2, 0, kNftNewchain);
    Ack(nft.get(), seq + 3, 0, kNftNewrule);

    errno = 0;
    EXPECT_EQ(-1, send_probe());
    EXPECT_EQ(EPERM, errno);
    pollfd ready{receiver.get(), POLLIN, 0};
    EXPECT_EQ(0, poll(&ready, 1, 150));
    auto query = Request(kNftGetrule, seq + 5);
    query[NLMSG_HDRLEN] = NFPROTO_IPV6;
    nlmsghdr header{};
    std::memcpy(&header, query.data(), sizeof(header));
    header.nlmsg_flags |= NLM_F_DUMP;
    std::memcpy(query.data(), &header, sizeof(header));
    AppendAttr(&query, NFTA_RULE_TABLE, table.c_str());
    AppendAttr(&query, NFTA_RULE_CHAIN, name);
    ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
    const auto rules = ReceiveRuleDump(nft.get(), seq + 5);
    ASSERT_EQ(1u, rules.size());
    EXPECT_TRUE(rules[0].has_counter);
    EXPECT_GE(rules[0].packets, 1u);

    auto remove = TableBatch(kNftDeltable, seq + 6, table.c_str(), NFPROTO_IPV6);
    ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
    Ack(nft.get(), seq + 7, 0, kNftDeltable);
    ASSERT_EQ(1, send_probe());
    ready.revents = 0;
    ASSERT_EQ(1, poll(&ready, 1, 1000));
    uint8_t received = 0;
    ASSERT_EQ(1, recv(receiver.get(), &received, 1, 0));
    EXPECT_EQ(probe, received);
  }
}

TEST(NetlinkNetfilter, NftCompatAddrtypeAllowsOmittedRuleCompat) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  ASSERT_GE(nft.get(), 0);
  const std::string table = "dkc035_addrtype_bare_" + std::to_string(getpid());
  constexpr uint32_t seq = 956;
  xt_addrtype_info_v1 addrtype{};
  addrtype.dest = XT_ADDRTYPE_LOCAL;
  std::vector<uint8_t> match;
  AppendNla(&match, NFTA_MATCH_NAME,
            reinterpret_cast<const uint8_t*>("addrtype"), sizeof("addrtype"));
  const uint32_t revision = htonl(1);
  AppendNla(&match, NFTA_MATCH_REV,
            reinterpret_cast<const uint8_t*>(&revision), sizeof(revision));
  AppendNla(&match, NFTA_MATCH_INFO,
            reinterpret_cast<const uint8_t*>(&addrtype), sizeof(addrtype));
  std::vector<uint8_t> expressions;
  AppendExpr(&expressions, "match", match);
  auto rule = RuleWithExpressions(seq + 3, table.c_str(), "input", expressions);
  auto batch = TableBatch(kNftNewtable, seq, table.c_str());
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto chain = NewChain(seq + 2, table.c_str(), "input", true, NF_ACCEPT,
                        NF_INET_LOCAL_IN);
  batch.insert(batch.end(), chain.begin(), chain.end());
  batch.insert(batch.end(), rule.begin(), rule.end());
  auto end = Request(NFNL_MSG_BATCH_END, seq + 4);
  batch.insert(batch.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewchain);
  Ack(nft.get(), seq + 3, 0, kNftNewrule);
  auto remove = TableBatch(kNftDeltable, seq + 5, table.c_str());
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
  Ack(nft.get(), seq + 6, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, NftCompatTcpMatchFiltersAndDumps) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  Fd blocked_listener(socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0));
  Fd allowed_listener(socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0));
  ASSERT_GE(nft.get(), 0);
  ASSERT_GE(blocked_listener.get(), 0);
  ASSERT_GE(allowed_listener.get(), 0);
  auto bind_listener = [](int fd) {
    sockaddr_in address{};
    address.sin_family = AF_INET;
    address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    if (bind(fd, reinterpret_cast<sockaddr*>(&address), sizeof(address)) != 0 ||
        listen(fd, 2) != 0) return sockaddr_in{};
    socklen_t length = sizeof(address);
    if (getsockname(fd, reinterpret_cast<sockaddr*>(&address), &length) != 0)
      return sockaddr_in{};
    return address;
  };
  const sockaddr_in blocked = bind_listener(blocked_listener.get());
  const sockaddr_in allowed = bind_listener(allowed_listener.get());
  ASSERT_NE(0, blocked.sin_port);
  ASSERT_NE(0, allowed.sin_port);
  auto connect_to = [](const sockaddr_in& destination) {
    const int client = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    if (client < 0) return client;
    const int result = connect(client, reinterpret_cast<const sockaddr*>(&destination),
                               sizeof(destination));
    if (result != 0 && errno != EINPROGRESS) { close(client); return -1; }
    return client;
  };
  pollfd blocked_ready{blocked_listener.get(), POLLIN, 0};
  {
    Fd baseline_client(connect_to(blocked));
    ASSERT_GE(baseline_client.get(), 0);
    ASSERT_EQ(1, poll(&blocked_ready, 1, 2000));
    Fd accepted(accept(blocked_listener.get(), nullptr, nullptr));
    ASSERT_GE(accepted.get(), 0);
  }

  xt_tcp tcp{};
  tcp.spts[1] = UINT16_MAX;
  tcp.dpts[0] = ntohs(blocked.sin_port);
  tcp.dpts[1] = tcp.dpts[0];
  std::vector<uint8_t> match;
  AppendNla(&match, NFTA_MATCH_NAME,
            reinterpret_cast<const uint8_t*>("tcp"), sizeof("tcp"));
  const uint32_t revision = htonl(0);
  AppendNla(&match, NFTA_MATCH_REV,
            reinterpret_cast<const uint8_t*>(&revision), sizeof(revision));
  AppendNla(&match, NFTA_MATCH_INFO,
            reinterpret_cast<const uint8_t*>(&tcp), sizeof(tcp));
  std::vector<uint8_t> expressions;
  AppendExpr(&expressions, "match", match);
  std::vector<uint8_t> verdict;
  const uint32_t drop = htonl(NF_DROP);
  AppendNla(&verdict, NFTA_VERDICT_CODE,
            reinterpret_cast<const uint8_t*>(&drop), sizeof(drop));
  std::vector<uint8_t> data;
  AppendNla(&data, NFTA_DATA_VERDICT | NLA_F_NESTED,
            verdict.data(), verdict.size());
  std::vector<uint8_t> immediate;
  const uint32_t verdict_reg = htonl(NFT_REG_VERDICT);
  AppendNla(&immediate, NFTA_IMMEDIATE_DREG,
            reinterpret_cast<const uint8_t*>(&verdict_reg), sizeof(verdict_reg));
  AppendNla(&immediate, NFTA_IMMEDIATE_DATA | NLA_F_NESTED,
            data.data(), data.size());
  AppendExpr(&expressions, "immediate", immediate);

  const std::string table = "dkc035_xttcp_" + std::to_string(getpid());
  constexpr uint32_t seq = 930;
  auto rule = RuleWithExpressions(seq + 3, table.c_str(), "input", expressions);
  std::vector<uint8_t> compat;
  const uint32_t protocol = htonl(IPPROTO_TCP);
  const uint32_t flags = 0;
  AppendNla(&compat, NFTA_RULE_COMPAT_PROTO,
            reinterpret_cast<const uint8_t*>(&protocol), sizeof(protocol));
  AppendNla(&compat, NFTA_RULE_COMPAT_FLAGS,
            reinterpret_cast<const uint8_t*>(&flags), sizeof(flags));
  AppendRawAttr(&rule, NFTA_RULE_COMPAT | NLA_F_NESTED,
                compat.data(), compat.size());
  auto batch = TableBatch(kNftNewtable, seq, table.c_str());
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto chain = NewChain(seq + 2, table.c_str(), "input", true, NF_ACCEPT,
                        NF_INET_LOCAL_IN);
  batch.insert(batch.end(), chain.begin(), chain.end());
  batch.insert(batch.end(), rule.begin(), rule.end());
  auto end = Request(NFNL_MSG_BATCH_END, seq + 4);
  batch.insert(batch.end(), end.begin(), end.end());

  bool installed = false;
  [&] {
    ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
    installed = true;
    Ack(nft.get(), seq + 1, 0, kNftNewtable);
    Ack(nft.get(), seq + 2, 0, kNftNewchain);
    Ack(nft.get(), seq + 3, 0, kNftNewrule);

    auto dump = Request(kNftGetrule, seq + 5);
    dump[NLMSG_HDRLEN] = NFPROTO_IPV4;
    nlmsghdr header{};
    std::memcpy(&header, dump.data(), sizeof(header));
    header.nlmsg_flags |= NLM_F_DUMP;
    std::memcpy(dump.data(), &header, sizeof(header));
    AppendAttr(&dump, NFTA_RULE_TABLE, table.c_str());
    AppendAttr(&dump, NFTA_RULE_CHAIN, "input");
    ASSERT_EQ(static_cast<ssize_t>(dump.size()), Send(nft.get(), dump));
    bool saw_match = false;
    bool done = false;
    for (int attempt = 0; attempt < 8 && !done; ++attempt) {
      pollfd readable{nft.get(), POLLIN, 0};
      ASSERT_EQ(1, poll(&readable, 1, 2000));
      std::array<uint8_t, 4096> bytes{};
      const ssize_t count = recv(nft.get(), bytes.data(), bytes.size(), 0);
      ASSERT_GE(count, static_cast<ssize_t>(NLMSG_HDRLEN));
      int remaining = count;
      for (auto* reply = reinterpret_cast<nlmsghdr*>(bytes.data());
           NLMSG_OK(reply, remaining); reply = NLMSG_NEXT(reply, remaining)) {
        if (reply->nlmsg_seq != seq + 5) continue;
        if (reply->nlmsg_type == NLMSG_DONE) { done = true; break; }
        ASSERT_EQ(kNftNewrule, reply->nlmsg_type);
        const auto* raw = reinterpret_cast<const uint8_t*>(reply);
        const auto attrs = NlaSpan{raw + NLMSG_HDRLEN + sizeof(nfgenmsg),
                                  reply->nlmsg_len - NLMSG_HDRLEN - sizeof(nfgenmsg)};
        const auto list = FindNla(attrs, NFTA_RULE_EXPRESSIONS);
        for (size_t offset = 0; offset + sizeof(nlattr) <= list.length;) {
          nlattr element{};
          std::memcpy(&element, list.data + offset, sizeof(element));
          ASSERT_GE(element.nla_len, sizeof(element));
          ASSERT_LE(offset + element.nla_len, list.length);
          const auto expression = NlaSpan{list.data + offset + sizeof(element),
                                          element.nla_len - sizeof(element)};
          const auto name = FindNla(expression, NFTA_EXPR_NAME);
          if (name.length == sizeof("match") &&
              std::memcmp(name.data, "match", name.length) == 0) {
            const auto attributes = FindNla(expression, NFTA_EXPR_DATA);
            const auto match_name = FindNla(attributes, NFTA_MATCH_NAME);
            const auto match_rev = FindNla(attributes, NFTA_MATCH_REV);
            const auto match_info = FindNla(attributes, NFTA_MATCH_INFO);
            ASSERT_EQ(sizeof("tcp"), match_name.length);
            EXPECT_EQ(0, std::memcmp(match_name.data, "tcp", match_name.length));
            EXPECT_EQ(0u, Be32(match_rev));
            ASSERT_EQ(16u, match_info.length);
            EXPECT_EQ(0, std::memcmp(match_info.data, &tcp, sizeof(tcp)));
            for (size_t index = sizeof(tcp); index < match_info.length; ++index)
              EXPECT_EQ(0, match_info.data[index]);
            saw_match = true;
          }
          offset += NLA_ALIGN(element.nla_len);
        }
      }
    }
    EXPECT_TRUE(done);
    EXPECT_TRUE(saw_match);

    Fd blocked_client(connect_to(blocked));
    ASSERT_GE(blocked_client.get(), 0);
    blocked_ready.revents = 0;
    EXPECT_EQ(0, poll(&blocked_ready, 1, 150));
    Fd allowed_client(connect_to(allowed));
    ASSERT_GE(allowed_client.get(), 0);
    pollfd allowed_ready{allowed_listener.get(), POLLIN, 0};
    ASSERT_EQ(1, poll(&allowed_ready, 1, 2000));
    Fd accepted(accept(allowed_listener.get(), nullptr, nullptr));
    EXPECT_GE(accepted.get(), 0);
    EXPECT_EQ(0, poll(&blocked_ready, 1, 0));
  }();
  if (installed) {
    auto remove = TableBatch(kNftDeltable, seq + 6, table.c_str());
    ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
    Ack(nft.get(), seq + 7, 0, kNftDeltable);
  }
}

TEST(NetlinkNetfilter, NftCompatTcpMatchRejectsMalformedInfoAndProtocol) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  ASSERT_GE(nft.get(), 0);
  enum class BadMatch {
    ShortInfo, LongInfo, WrongProtocol, MissingFlags, InvalidInverse,
    MissingName, MissingRevision, MissingInfo, UnknownName,
    UnknownRevision, RevisionAboveLimit,
  };
  uint32_t seq = 950;
  auto reject = [&](const char* suffix, BadMatch issue, int expected_error) {
    const std::string table = std::string("dkc035_badxt_") + suffix;
    xt_tcp tcp{};
    tcp.spts[1] = UINT16_MAX;
    tcp.dpts[1] = UINT16_MAX;
    tcp.invflags = issue == BadMatch::InvalidInverse ? 0x80 : 0;
    const size_t info_length = issue == BadMatch::ShortInfo ? sizeof(tcp) - 1 :
                               issue == BadMatch::LongInfo ? 17 : sizeof(tcp);
    std::vector<uint8_t> info(info_length, 0);
    std::memcpy(info.data(), &tcp, std::min(info_length, sizeof(tcp)));
    std::vector<uint8_t> match;
    if (issue != BadMatch::MissingName) {
      const char* name = issue == BadMatch::UnknownName ? "no_such_xt" : "tcp";
      AppendNla(&match, NFTA_MATCH_NAME,
                reinterpret_cast<const uint8_t*>(name), std::strlen(name) + 1);
    }
    if (issue != BadMatch::MissingRevision) {
      const uint32_t revision = htonl(issue == BadMatch::UnknownRevision ? 1 :
                                      issue == BadMatch::RevisionAboveLimit ? 256 : 0);
      AppendNla(&match, NFTA_MATCH_REV,
                reinterpret_cast<const uint8_t*>(&revision), sizeof(revision));
    }
    if (issue != BadMatch::MissingInfo)
      AppendNla(&match, NFTA_MATCH_INFO, info.data(), info.size());
    std::vector<uint8_t> expressions;
    AppendExpr(&expressions, "match", match);
    auto rule = RuleWithExpressions(seq + 3, table.c_str(), "input", expressions);
    std::vector<uint8_t> compat;
    const uint32_t be_protocol = htonl(issue == BadMatch::WrongProtocol ?
                                        IPPROTO_UDP : IPPROTO_TCP);
    AppendNla(&compat, NFTA_RULE_COMPAT_PROTO,
              reinterpret_cast<const uint8_t*>(&be_protocol), sizeof(be_protocol));
    if (issue != BadMatch::MissingFlags) {
      const uint32_t flags = 0;
      AppendNla(&compat, NFTA_RULE_COMPAT_FLAGS,
                reinterpret_cast<const uint8_t*>(&flags), sizeof(flags));
    }
    AppendRawAttr(&rule, NFTA_RULE_COMPAT | NLA_F_NESTED,
                  compat.data(), compat.size());

    auto batch = TableBatch(kNftNewtable, seq, table.c_str());
    batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
    auto chain = NewChain(seq + 2, table.c_str(), "input", true, NF_ACCEPT,
                          NF_INET_LOCAL_IN);
    batch.insert(batch.end(), chain.begin(), chain.end());
    batch.insert(batch.end(), rule.begin(), rule.end());
    auto end = Request(NFNL_MSG_BATCH_END, seq + 4);
    batch.insert(batch.end(), end.begin(), end.end());
    ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
    Ack(nft.get(), seq + 1, 0, kNftNewtable);
    Ack(nft.get(), seq + 2, 0, kNftNewchain);
    Ack(nft.get(), seq + 3, expected_error, kNftNewrule);

    auto query = Request(kNftGettable, seq + 5);
    query[NLMSG_HDRLEN] = NFPROTO_IPV4;
    AppendAttr(&query, NFTA_TABLE_NAME, table.c_str());
    ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
    Ack(nft.get(), seq + 5, ENOENT, kNftGettable);
    seq += 10;
  };
  reject("short", BadMatch::ShortInfo, EINVAL);
  reject("long", BadMatch::LongInfo, EINVAL);
  reject("proto", BadMatch::WrongProtocol, EINVAL);
  reject("flags", BadMatch::MissingFlags, EINVAL);
  reject("inv", BadMatch::InvalidInverse, EINVAL);
  reject("noname", BadMatch::MissingName, EINVAL);
  reject("norev", BadMatch::MissingRevision, EINVAL);
  reject("noinfo", BadMatch::MissingInfo, EINVAL);
  reject("name", BadMatch::UnknownName, ENOENT);
  reject("rev", BadMatch::UnknownRevision, ENOENT);
  reject("range", BadMatch::RevisionAboveLimit, ERANGE);
}

TEST(NetlinkNetfilter, NativeTransportPayloadDropsOnlyMatchingUdpPort) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  Fd receiver(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd other(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd sender(socket(AF_INET, SOCK_DGRAM, 0));
  ASSERT_GE(nft.get(), 0);
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(other.get(), 0);
  ASSERT_GE(sender.get(), 0);

  auto bind_loopback = [](int fd) {
    sockaddr_in address{};
    address.sin_family = AF_INET;
    address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    if (bind(fd, reinterpret_cast<sockaddr*>(&address), sizeof(address)) != 0)
      return sockaddr_in{};
    socklen_t length = sizeof(address);
    if (getsockname(fd, reinterpret_cast<sockaddr*>(&address), &length) != 0)
      return sockaddr_in{};
    return address;
  };
  const sockaddr_in blocked = bind_loopback(receiver.get());
  const sockaddr_in allowed = bind_loopback(other.get());
  ASSERT_NE(0, blocked.sin_port);
  ASSERT_NE(0, allowed.sin_port);
  const uint8_t probe = 0x35;
  ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                      reinterpret_cast<const sockaddr*>(&blocked), sizeof(blocked)));
  pollfd blocked_ready{receiver.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&blocked_ready, 1, 2000));
  uint8_t received = 0;
  ASSERT_EQ(1, recv(receiver.get(), &received, 1, 0));
  ASSERT_EQ(probe, received);

  std::vector<uint8_t> expressions;
  auto append_equal_payload = [&](uint32_t base, uint32_t offset,
                                  const uint8_t* expected, size_t size) {
    std::vector<uint8_t> payload;
    const uint32_t register_id = htonl(NFT_REG_1);
    const uint32_t payload_base = htonl(base);
    const uint32_t payload_offset = htonl(offset);
    const uint32_t payload_length = htonl(size);
    AppendNla(&payload, NFTA_PAYLOAD_DREG,
              reinterpret_cast<const uint8_t*>(&register_id), sizeof(register_id));
    AppendNla(&payload, NFTA_PAYLOAD_BASE,
              reinterpret_cast<const uint8_t*>(&payload_base), sizeof(payload_base));
    AppendNla(&payload, NFTA_PAYLOAD_OFFSET,
              reinterpret_cast<const uint8_t*>(&payload_offset), sizeof(payload_offset));
    AppendNla(&payload, NFTA_PAYLOAD_LEN,
              reinterpret_cast<const uint8_t*>(&payload_length), sizeof(payload_length));
    AppendExpr(&expressions, "payload", payload);
    std::vector<uint8_t> comparison;
    const uint32_t op = htonl(NFT_CMP_EQ);
    AppendNla(&comparison, NFTA_CMP_SREG,
              reinterpret_cast<const uint8_t*>(&register_id), sizeof(register_id));
    AppendNla(&comparison, NFTA_CMP_OP,
              reinterpret_cast<const uint8_t*>(&op), sizeof(op));
    std::vector<uint8_t> value;
    AppendNla(&value, NFTA_DATA_VALUE, expected, size);
    AppendNla(&comparison, NFTA_CMP_DATA | NLA_F_NESTED,
              value.data(), value.size());
    AppendExpr(&expressions, "cmp", comparison);
  };
  const uint8_t udp = IPPROTO_UDP;
  append_equal_payload(NFT_PAYLOAD_NETWORK_HEADER, 9, &udp, sizeof(udp));
  append_equal_payload(NFT_PAYLOAD_TRANSPORT_HEADER, 2,
                       reinterpret_cast<const uint8_t*>(&blocked.sin_port),
                       sizeof(blocked.sin_port));
  std::vector<uint8_t> verdict;
  const uint32_t drop = htonl(NF_DROP);
  AppendNla(&verdict, NFTA_VERDICT_CODE,
            reinterpret_cast<const uint8_t*>(&drop), sizeof(drop));
  std::vector<uint8_t> data;
  AppendNla(&data, NFTA_DATA_VERDICT | NLA_F_NESTED,
            verdict.data(), verdict.size());
  std::vector<uint8_t> immediate;
  const uint32_t verdict_reg = htonl(NFT_REG_VERDICT);
  AppendNla(&immediate, NFTA_IMMEDIATE_DREG,
            reinterpret_cast<const uint8_t*>(&verdict_reg), sizeof(verdict_reg));
  AppendNla(&immediate, NFTA_IMMEDIATE_DATA | NLA_F_NESTED,
            data.data(), data.size());
  AppendExpr(&expressions, "immediate", immediate);

  const std::string table = "dkc035_dport_" + std::to_string(getpid());
  constexpr uint32_t seq = 926;
  auto batch = TableBatch(kNftNewtable, seq, table.c_str());
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto chain = NewChain(seq + 2, table.c_str(), "input", true, NF_ACCEPT,
                        NF_INET_LOCAL_IN);
  auto rule = RuleWithExpressions(seq + 3, table.c_str(), "input", expressions);
  batch.insert(batch.end(), chain.begin(), chain.end());
  batch.insert(batch.end(), rule.begin(), rule.end());
  auto end = Request(NFNL_MSG_BATCH_END, seq + 4);
  batch.insert(batch.end(), end.begin(), end.end());
  bool installed = false;
  [&] {
    ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
    installed = true;
    Ack(nft.get(), seq + 1, 0, kNftNewtable);
    Ack(nft.get(), seq + 2, 0, kNftNewchain);
    Ack(nft.get(), seq + 3, 0, kNftNewrule);

    EXPECT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                        reinterpret_cast<const sockaddr*>(&blocked), sizeof(blocked)));
    EXPECT_EQ(0, poll(&blocked_ready, 1, 150));
    EXPECT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                        reinterpret_cast<const sockaddr*>(&allowed), sizeof(allowed)));
    pollfd allowed_ready{other.get(), POLLIN, 0};
    EXPECT_EQ(1, poll(&allowed_ready, 1, 2000));
    EXPECT_EQ(0, poll(&blocked_ready, 1, 0));
    EXPECT_EQ(1, recv(other.get(), &received, 1, 0));
    EXPECT_EQ(probe, received);
  }();
  if (installed) {
    auto remove = TableBatch(kNftDeltable, seq + 5, table.c_str());
    ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
    Ack(nft.get(), seq + 6, 0, kNftDeltable);
    ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                        reinterpret_cast<const sockaddr*>(&blocked), sizeof(blocked)));
    ASSERT_EQ(1, poll(&blocked_ready, 1, 2000));
    ASSERT_EQ(1, recv(receiver.get(), &received, 1, 0));
    EXPECT_EQ(probe, received);
  }
}

TEST(NetlinkNetfilter, NftOutputAndPostroutingDropSynchronouslyRejectUdpSendto) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  Fd receiver(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd sender(socket(AF_INET, SOCK_DGRAM, 0));
  ASSERT_GE(nft.get(), 0);
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(sender.get(), 0);
  sockaddr_in destination{};
  destination.sin_family = AF_INET;
  destination.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  ASSERT_EQ(0, bind(receiver.get(), reinterpret_cast<sockaddr*>(&destination),
                    sizeof(destination)));
  socklen_t address_length = sizeof(destination);
  ASSERT_EQ(0, getsockname(receiver.get(),
                           reinterpret_cast<sockaddr*>(&destination), &address_length));
  const uint8_t probe = 0x35;
  pollfd ready{receiver.get(), POLLIN, 0};
  auto send_and_receive = [&] {
    ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                        reinterpret_cast<sockaddr*>(&destination), address_length));
    ready.revents = 0;
    ASSERT_EQ(1, poll(&ready, 1, 2000));
    uint8_t received = 0;
    ASSERT_EQ(1, recv(receiver.get(), &received, 1, 0));
    EXPECT_EQ(probe, received);
  };
  send_and_receive();

  for (const auto [hook, name] : {
           std::pair<uint32_t, const char*>{NF_INET_LOCAL_OUT, "output"},
           {NF_INET_POST_ROUTING, "postrouting"},
       }) {
    const std::string table = std::string("dkc035_") + name + "_" +
                              std::to_string(getpid());
    const uint32_t seq = hook == NF_INET_LOCAL_OUT ? 970 : 980;
    auto batch = TableBatch(kNftNewtable, seq, table.c_str());
    batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
    auto chain = NewChain(seq + 2, table.c_str(), name, true, NF_DROP, hook);
    batch.insert(batch.end(), chain.begin(), chain.end());
    auto end = Request(NFNL_MSG_BATCH_END, seq + 3);
    batch.insert(batch.end(), end.begin(), end.end());
    bool installed = false;
    [&] {
      ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
      installed = true;
      Ack(nft.get(), seq + 1, 0, kNftNewtable);
      Ack(nft.get(), seq + 2, 0, kNftNewchain);
      errno = 0;
      const ssize_t rejected = sendto(sender.get(), &probe, 1, 0,
                                      reinterpret_cast<sockaddr*>(&destination),
                                      address_length);
      const int send_error = errno;
      EXPECT_EQ(-1, rejected);
      EXPECT_EQ(EPERM, send_error);
      ready.revents = 0;
      EXPECT_EQ(0, poll(&ready, 1, 150));
    }();
    if (installed) {
      auto remove = TableBatch(kNftDeltable, seq + 4, table.c_str());
      ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
      Ack(nft.get(), seq + 5, 0, kNftDeltable);
      send_and_receive();
    }
  }
}

TEST(NetlinkNetfilter, NftIpv6OutputAndPostroutingDropSynchronouslyRejectUdpSendto) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  Fd receiver(socket(AF_INET6, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd sender(socket(AF_INET6, SOCK_DGRAM, 0));
  ASSERT_GE(nft.get(), 0);
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(sender.get(), 0);
  sockaddr_in6 destination{};
  destination.sin6_family = AF_INET6;
  destination.sin6_addr = in6addr_loopback;
  ASSERT_EQ(0, bind(receiver.get(), reinterpret_cast<sockaddr*>(&destination),
                    sizeof(destination)));
  socklen_t address_length = sizeof(destination);
  ASSERT_EQ(0, getsockname(receiver.get(),
                           reinterpret_cast<sockaddr*>(&destination), &address_length));
  const uint8_t probe = 0x36;
  pollfd ready{receiver.get(), POLLIN, 0};
  auto send_and_receive = [&] {
    ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                        reinterpret_cast<sockaddr*>(&destination), address_length));
    ready.revents = 0;
    ASSERT_EQ(1, poll(&ready, 1, 2000));
    uint8_t received = 0;
    ASSERT_EQ(1, recv(receiver.get(), &received, 1, 0));
    EXPECT_EQ(probe, received);
  };
  send_and_receive();

  for (const auto [hook, name] : {
           std::pair<uint32_t, const char*>{NF_INET_LOCAL_OUT, "output"},
           {NF_INET_POST_ROUTING, "postrouting"},
       }) {
    const std::string table = std::string("dkc035_ip6_") + name + "_" +
                              std::to_string(getpid());
    const uint32_t seq = hook == NF_INET_LOCAL_OUT ? 990 : 1000;
    auto batch = TableBatch(kNftNewtable, seq, table.c_str(), NFPROTO_IPV6);
    batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
    auto chain = NewChain(seq + 2, table.c_str(), name, true, NF_DROP, hook,
                          NFPROTO_IPV6);
    batch.insert(batch.end(), chain.begin(), chain.end());
    auto end = Request(NFNL_MSG_BATCH_END, seq + 3);
    batch.insert(batch.end(), end.begin(), end.end());
    bool installed = false;
    [&] {
      ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
      installed = true;
      Ack(nft.get(), seq + 1, 0, kNftNewtable);
      Ack(nft.get(), seq + 2, 0, kNftNewchain);
      errno = 0;
      const ssize_t rejected = sendto(sender.get(), &probe, 1, 0,
                                      reinterpret_cast<sockaddr*>(&destination),
                                      address_length);
      const int send_error = errno;
      EXPECT_EQ(-1, rejected);
      EXPECT_EQ(EPERM, send_error);
      ready.revents = 0;
      EXPECT_EQ(0, poll(&ready, 1, 150));
    }();
    if (installed) {
      auto remove = TableBatch(kNftDeltable, seq + 4, table.c_str(), NFPROTO_IPV6);
      ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
      Ack(nft.get(), seq + 5, 0, kNftDeltable);
      send_and_receive();
    }
  }
}

TEST(NetlinkNetfilter, NftOutputAndPostroutingDropSynchronouslyRejectRawSendto) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  Fd plain(socket(AF_INET, SOCK_RAW, 253));
  Fd hdrincl(socket(AF_INET, SOCK_RAW, IPPROTO_RAW));
  ASSERT_GE(nft.get(), 0);
  ASSERT_GE(plain.get(), 0);
  ASSERT_GE(hdrincl.get(), 0);
  sockaddr_in destination{};
  destination.sin_family = AF_INET;
  destination.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  const std::array<uint8_t, 8> payload{0x35, 0, 0, 0, 0, 0, 0, 0};
  std::array<uint8_t, 28> packet{};
  packet[0] = 0x45;
  packet[8] = 64;
  packet[9] = 253;
  packet[16] = 192;
  packet[17] = 0;
  packet[18] = 2;
  packet[19] = 1;

  for (const auto [hook, name] : {
           std::pair<uint32_t, const char*>{NF_INET_LOCAL_OUT, "raw_output"},
           {NF_INET_POST_ROUTING, "raw_postrouting"},
       }) {
    const std::string table = std::string("dkc035_") + name + "_" +
                              std::to_string(getpid());
    const uint32_t seq = hook == NF_INET_LOCAL_OUT ? 985 : 995;
    auto batch = TableBatch(kNftNewtable, seq, table.c_str());
    batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
    auto chain = NewChain(seq + 2, table.c_str(), name, true, NF_DROP, hook);
    batch.insert(batch.end(), chain.begin(), chain.end());
    auto end = Request(NFNL_MSG_BATCH_END, seq + 3);
    batch.insert(batch.end(), end.begin(), end.end());
    bool installed = false;
    [&] {
      ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
      installed = true;
      Ack(nft.get(), seq + 1, 0, kNftNewtable);
      Ack(nft.get(), seq + 2, 0, kNftNewchain);
      for (const auto [fd, bytes, len] : {
               std::tuple<int, const uint8_t*, size_t>{plain.get(), payload.data(),
                                                        payload.size()},
               {hdrincl.get(), packet.data(), packet.size()},
           }) {
        errno = 0;
        EXPECT_EQ(-1, sendto(fd, bytes, len, 0,
                             reinterpret_cast<sockaddr*>(&destination),
                             sizeof(destination)));
        EXPECT_EQ(EPERM, errno);
      }
    }();
    if (installed) {
      auto remove = TableBatch(kNftDeltable, seq + 4, table.c_str());
      ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
      Ack(nft.get(), seq + 5, 0, kNftDeltable);
    }
  }
}

TEST(NetlinkNetfilter, NftIpv6OutputAndPostroutingDropSynchronouslyRejectRawSendto) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  Fd plain(socket(AF_INET6, SOCK_RAW, 253));
  Fd hdrincl(socket(AF_INET6, SOCK_RAW, 253));
  ASSERT_GE(nft.get(), 0);
  ASSERT_GE(plain.get(), 0);
  ASSERT_GE(hdrincl.get(), 0);
  const int enabled = 1;
  ASSERT_EQ(0, setsockopt(hdrincl.get(), IPPROTO_IPV6, IPV6_HDRINCL,
                          &enabled, sizeof(enabled)));
  sockaddr_in6 destination{};
  destination.sin6_family = AF_INET6;
  destination.sin6_addr = in6addr_loopback;
  const std::array<uint8_t, 8> payload{0x36, 0, 0, 0, 0, 0, 0, 0};
  std::array<uint8_t, 48> packet{};
  packet[0] = 0x60;
  packet[5] = payload.size();
  packet[6] = 253;
  packet[7] = 64;
  packet[23] = 1;  // ::1 source
  packet[39] = 1;  // ::1 destination
  std::copy(payload.begin(), payload.end(), packet.begin() + 40);

  for (const auto [hook, name] : {
           std::pair<uint32_t, const char*>{NF_INET_LOCAL_OUT, "raw6_output"},
           {NF_INET_POST_ROUTING, "raw6_postrouting"},
       }) {
    const std::string table = std::string("dkc035_") + name + "_" +
                              std::to_string(getpid());
    const uint32_t seq = hook == NF_INET_LOCAL_OUT ? 1010 : 1020;
    auto batch = TableBatch(kNftNewtable, seq, table.c_str(), NFPROTO_IPV6);
    batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
    auto chain = NewChain(seq + 2, table.c_str(), name, true, NF_DROP, hook,
                          NFPROTO_IPV6);
    batch.insert(batch.end(), chain.begin(), chain.end());
    auto end = Request(NFNL_MSG_BATCH_END, seq + 3);
    batch.insert(batch.end(), end.begin(), end.end());
    bool installed = false;
    [&] {
      ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
      installed = true;
      Ack(nft.get(), seq + 1, 0, kNftNewtable);
      Ack(nft.get(), seq + 2, 0, kNftNewchain);
      for (const auto [fd, bytes, len] : {
               std::tuple<int, const uint8_t*, size_t>{plain.get(), payload.data(),
                                                        payload.size()},
               {hdrincl.get(), packet.data(), packet.size()},
           }) {
        errno = 0;
        EXPECT_EQ(-1, sendto(fd, bytes, len, 0,
                             reinterpret_cast<sockaddr*>(&destination),
                             sizeof(destination)));
        EXPECT_EQ(EPERM, errno);
      }
    }();
    if (installed) {
      auto remove = TableBatch(kNftDeltable, seq + 4, table.c_str(), NFPROTO_IPV6);
      ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
      Ack(nft.get(), seq + 5, 0, kNftDeltable);
    }
  }
}

TEST(NetlinkNetfilter, UdpUnconnectedSendBufferTracksPendingOutputReadiness) {
  ASSERT_NE(0u, if_nametoindex("veth1"));
  Fd sender(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd epoll_fd(epoll_create1(EPOLL_CLOEXEC));
  ASSERT_GE(sender.get(), 0);
  ASSERT_GE(epoll_fd.get(), 0);
  constexpr char egress[] = "veth1";
  ASSERT_EQ(0, setsockopt(sender.get(), SOL_SOCKET, SO_BINDTODEVICE,
                          egress, sizeof(egress)));
  const int requested_buffer = 2304;
  ASSERT_EQ(0, setsockopt(sender.get(), SOL_SOCKET, SO_SNDBUF,
                          &requested_buffer, sizeof(requested_buffer)));
  int send_buffer = 0;
  socklen_t option_length = sizeof(send_buffer);
  ASSERT_EQ(0, getsockopt(sender.get(), SOL_SOCKET, SO_SNDBUF,
                          &send_buffer, &option_length));
  ASSERT_EQ(sizeof(send_buffer), option_length);
  ASSERT_EQ(4608, send_buffer);  // Linux's minimum doubled send buffer.

  epoll_event interest{};
  interest.events = EPOLLOUT | EPOLLET;
  interest.data.fd = sender.get();
  ASSERT_EQ(0, epoll_ctl(epoll_fd.get(), EPOLL_CTL_ADD, sender.get(), &interest));
  epoll_event notification{};
  ASSERT_EQ(1, epoll_wait(epoll_fd.get(), &notification, 1, 0));
  EXPECT_NE(0u, notification.events & EPOLLOUT);
  EXPECT_EQ(0, epoll_wait(epoll_fd.get(), &notification, 1, 0));
  pollfd writable{sender.get(), POLLOUT, 0};
  ASSERT_EQ(1, poll(&writable, 1, 0));
  EXPECT_NE(0, writable.revents & POLLOUT);

  // Nobody owns this address on the fixture /24. Linux retains the queued
  // packet while ARP probes run, then releases its socket send-memory charge.
  sockaddr_in destination{};
  destination.sin_family = AF_INET;
  destination.sin_port = htons(34535);
  ASSERT_EQ(1, inet_pton(AF_INET, "111.111.11.253", &destination.sin_addr));
  std::array<uint8_t, 4000> payload{};
  ASSERT_EQ(static_cast<ssize_t>(payload.size()),
            sendto(sender.get(), payload.data(), payload.size(), 0,
                   reinterpret_cast<sockaddr*>(&destination), sizeof(destination)));
  writable.revents = 0;
  EXPECT_EQ(0, poll(&writable, 1, 0));
  EXPECT_EQ(0, epoll_wait(epoll_fd.get(), &notification, 1, 0));

  ASSERT_EQ(1, epoll_wait(epoll_fd.get(), &notification, 1, 10000));
  EXPECT_NE(0u, notification.events & EPOLLOUT);
  writable.revents = 0;
  ASSERT_EQ(1, poll(&writable, 1, 0));
  EXPECT_NE(0, writable.revents & POLLOUT);
}

TEST(NetlinkNetfilter, NftPostroutingCountsMulticastCloneAndOriginalSeparately) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  // A non-loopback output device is required: Linux routes multicast sent
  // directly through lo without ip_mc_output's clone-and-original branch.
  const unsigned egress_ifindex = if_nametoindex("veth1");
  ASSERT_NE(0u, egress_ifindex);
  Fd nft(Open());
  Fd receiver(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd sender(socket(AF_INET, SOCK_DGRAM, 0));
  ASSERT_GE(nft.get(), 0);
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(sender.get(), 0);
  sockaddr_in destination{};
  destination.sin_family = AF_INET;
  destination.sin_port = htons(38498);
  ASSERT_EQ(1, inet_pton(AF_INET, "239.255.35.2", &destination.sin_addr));
  sockaddr_in bind_address{};
  bind_address.sin_family = AF_INET;
  bind_address.sin_port = destination.sin_port;
  bind_address.sin_addr.s_addr = htonl(INADDR_ANY);
  ASSERT_EQ(0, bind(receiver.get(), reinterpret_cast<sockaddr*>(&bind_address),
                    sizeof(bind_address)));
  ip_mreqn membership{};
  membership.imr_multiaddr = destination.sin_addr;
  membership.imr_ifindex = static_cast<int>(egress_ifindex);
  ASSERT_EQ(0, setsockopt(receiver.get(), IPPROTO_IP, IP_ADD_MEMBERSHIP,
                          &membership, sizeof(membership)));
  ip_mreqn outbound{};
  outbound.imr_ifindex = static_cast<int>(egress_ifindex);
  ASSERT_EQ(0, setsockopt(sender.get(), IPPROTO_IP, IP_MULTICAST_IF,
                          &outbound, sizeof(outbound)));
  const uint8_t probe = 0x35;
  pollfd ready{receiver.get(), POLLIN, 0};
  auto send_and_receive = [&] {
    ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                        reinterpret_cast<sockaddr*>(&destination), sizeof(destination)));
    ready.revents = 0;
    ASSERT_EQ(1, poll(&ready, 1, 2000));
    uint8_t received = 0;
    ASSERT_EQ(1, recv(receiver.get(), &received, 1, 0));
    EXPECT_EQ(probe, received);
    ready.revents = 0;
    EXPECT_EQ(0, poll(&ready, 1, 0));
  };
  send_and_receive();

  const std::string table = "dkc035_mc_post_" + std::to_string(getpid());
  constexpr uint32_t seq = 990;
  auto batch = TableBatch(kNftNewtable, seq, table.c_str());
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto chain = NewChain(seq + 2, table.c_str(), "postrouting", true, NF_ACCEPT,
                        NF_INET_POST_ROUTING);
  auto rule = ProtocolCounterRule(seq + 3, table.c_str(), "postrouting", NFT_CMP_EQ,
                                  NFT_PAYLOAD_NETWORK_HEADER, NFT_REG_1, 9,
                                  UINT32_MAX, false, 1, false, &destination.sin_addr);
  batch.insert(batch.end(), chain.begin(), chain.end());
  batch.insert(batch.end(), rule.begin(), rule.end());
  auto end = Request(NFNL_MSG_BATCH_END, seq + 4);
  batch.insert(batch.end(), end.begin(), end.end());
  bool installed = false;
  [&] {
    ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
    installed = true;
    Ack(nft.get(), seq + 1, 0, kNftNewtable);
    Ack(nft.get(), seq + 2, 0, kNftNewchain);
    Ack(nft.get(), seq + 3, 0, kNftNewrule);
    auto packet_count = [&](uint32_t query_seq) {
      auto query = Request(kNftGetrule, query_seq);
      query[NLMSG_HDRLEN] = NFPROTO_IPV4;
      nlmsghdr header{};
      std::memcpy(&header, query.data(), sizeof(header));
      header.nlmsg_flags |= NLM_F_DUMP;
      std::memcpy(query.data(), &header, sizeof(header));
      AppendAttr(&query, NFTA_RULE_TABLE, table.c_str());
      AppendAttr(&query, NFTA_RULE_CHAIN, "postrouting");
      if (Send(nft.get(), query) != static_cast<ssize_t>(query.size())) {
        ADD_FAILURE() << "cannot query postrouting counter";
        return uint64_t{0};
      }
      const auto rules = ReceiveRuleDump(nft.get(), query_seq);
      if (rules.size() != 1 || !rules[0].has_counter) {
        ADD_FAILURE() << "missing postrouting counter";
        return uint64_t{0};
      }
      return rules[0].packets;
    };
    const uint64_t before = packet_count(seq + 5);
    send_and_receive();
    const uint64_t with_loopback = packet_count(seq + 6);
    EXPECT_EQ(before + 2, with_loopback);

    const uint8_t ttl_zero = 0;
    ASSERT_EQ(0, setsockopt(sender.get(), IPPROTO_IP, IP_MULTICAST_TTL,
                            &ttl_zero, sizeof(ttl_zero)));
    send_and_receive();
    const uint64_t ttl_zero_count = packet_count(seq + 7);
    EXPECT_EQ(with_loopback + 1, ttl_zero_count);
  }();
  if (installed) {
    auto remove = TableBatch(kNftDeltable, seq + 8, table.c_str());
    ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
    Ack(nft.get(), seq + 9, 0, kNftDeltable);
  }
}

TEST(NetlinkNetfilter, NftPostroutingCountsBroadcastCloneAndOriginalSeparately) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  ASSERT_NE(0u, if_nametoindex("veth1"));
  Fd nft(Open());
  Fd receiver(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd sender(socket(AF_INET, SOCK_DGRAM, 0));
  ASSERT_GE(nft.get(), 0);
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(sender.get(), 0);
  sockaddr_in destination{};
  destination.sin_family = AF_INET;
  destination.sin_port = htons(38499);
  ASSERT_EQ(1, inet_pton(AF_INET, "111.111.11.255", &destination.sin_addr));
  sockaddr_in bind_address{};
  bind_address.sin_family = AF_INET;
  bind_address.sin_port = destination.sin_port;
  bind_address.sin_addr.s_addr = htonl(INADDR_ANY);
  ASSERT_EQ(0, bind(receiver.get(), reinterpret_cast<sockaddr*>(&bind_address),
                    sizeof(bind_address)));
  const int enabled = 1;
  ASSERT_EQ(0, setsockopt(sender.get(), SOL_SOCKET, SO_BROADCAST,
                          &enabled, sizeof(enabled)));
  constexpr char egress[] = "veth1";
  ASSERT_EQ(0, setsockopt(sender.get(), SOL_SOCKET, SO_BINDTODEVICE,
                          egress, sizeof(egress)));
  pollfd ready{receiver.get(), POLLIN, 0};
  auto send_and_receive = [&](uint8_t probe) {
    ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                        reinterpret_cast<sockaddr*>(&destination), sizeof(destination)));
    bool saw_probe = false;
    for (int attempt = 0; attempt < 4; ++attempt) {
      ready.revents = 0;
      const int events = poll(&ready, 1, attempt == 0 ? 2000 : 50);
      if (events == 0) break;
      ASSERT_EQ(1, events);
      uint8_t received = 0;
      ASSERT_EQ(1, recv(receiver.get(), &received, 1, 0));
      EXPECT_EQ(probe, received);
      saw_probe = true;
    }
    EXPECT_TRUE(saw_probe);
  };
  send_and_receive(0x34);

  const std::string table = "dkc035_bc_post_" + std::to_string(getpid());
  constexpr uint32_t seq = 1000;
  auto batch = TableBatch(kNftNewtable, seq, table.c_str());
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto chain = NewChain(seq + 2, table.c_str(), "postrouting", true, NF_ACCEPT,
                        NF_INET_POST_ROUTING);
  auto rule = ProtocolCounterRule(seq + 3, table.c_str(), "postrouting", NFT_CMP_EQ,
                                  NFT_PAYLOAD_NETWORK_HEADER, NFT_REG_1, 9,
                                  UINT32_MAX, false, 1, false, &destination.sin_addr);
  batch.insert(batch.end(), chain.begin(), chain.end());
  batch.insert(batch.end(), rule.begin(), rule.end());
  auto end = Request(NFNL_MSG_BATCH_END, seq + 4);
  batch.insert(batch.end(), end.begin(), end.end());
  bool installed = false;
  [&] {
    ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
    installed = true;
    Ack(nft.get(), seq + 1, 0, kNftNewtable);
    Ack(nft.get(), seq + 2, 0, kNftNewchain);
    Ack(nft.get(), seq + 3, 0, kNftNewrule);
    auto query = Request(kNftGetrule, seq + 5);
    query[NLMSG_HDRLEN] = NFPROTO_IPV4;
    nlmsghdr header{};
    std::memcpy(&header, query.data(), sizeof(header));
    header.nlmsg_flags |= NLM_F_DUMP;
    std::memcpy(query.data(), &header, sizeof(header));
    AppendAttr(&query, NFTA_RULE_TABLE, table.c_str());
    AppendAttr(&query, NFTA_RULE_CHAIN, "postrouting");
    ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
    const auto before = ReceiveRuleDump(nft.get(), seq + 5);
    ASSERT_EQ(1u, before.size());
    ASSERT_TRUE(before[0].has_counter);

    send_and_receive(0x35);
    std::memcpy(&header, query.data(), sizeof(header));
    header.nlmsg_seq = seq + 6;
    std::memcpy(query.data(), &header, sizeof(header));
    ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
    const auto after = ReceiveRuleDump(nft.get(), seq + 6);
    ASSERT_EQ(1u, after.size());
    ASSERT_TRUE(after[0].has_counter);
    EXPECT_EQ(before[0].packets + 2, after[0].packets);
  }();
  if (installed) {
    auto remove = TableBatch(kNftDeltable, seq + 7, table.c_str());
    ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
    Ack(nft.get(), seq + 8, 0, kNftDeltable);
  }
}

TEST(NetlinkNetfilter, NftPreroutingDropBlocksLocalIpv4Multicast) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  const unsigned loopback_ifindex = if_nametoindex("lo");
  ASSERT_NE(0u, loopback_ifindex);
  Fd receiver(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd sender(socket(AF_INET, SOCK_DGRAM, 0));
  Fd nft(Open());
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(sender.get(), 0);
  ASSERT_GE(nft.get(), 0);

  sockaddr_in address{};
  address.sin_family = AF_INET;
  address.sin_port = htons(38497);
  ASSERT_EQ(1, inet_pton(AF_INET, "239.255.35.1", &address.sin_addr));
  sockaddr_in bind_address{};
  bind_address.sin_family = AF_INET;
  bind_address.sin_port = address.sin_port;
  bind_address.sin_addr.s_addr = htonl(INADDR_ANY);
  ASSERT_EQ(0, bind(receiver.get(), reinterpret_cast<sockaddr*>(&bind_address),
                    sizeof(bind_address)));
  ip_mreqn membership{};
  membership.imr_multiaddr = address.sin_addr;
  membership.imr_ifindex = static_cast<int>(loopback_ifindex);
  ASSERT_EQ(0, setsockopt(receiver.get(), IPPROTO_IP, IP_ADD_MEMBERSHIP,
                          &membership, sizeof(membership)));
  ip_mreqn outbound{};
  outbound.imr_ifindex = static_cast<int>(loopback_ifindex);
  ASSERT_EQ(0, setsockopt(sender.get(), IPPROTO_IP, IP_MULTICAST_IF,
                          &outbound, sizeof(outbound)));

  const uint8_t probe = 0x35;
  ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                      reinterpret_cast<sockaddr*>(&address), sizeof(address)));
  pollfd ready{receiver.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 2000));
  uint8_t received = 0;
  ASSERT_EQ(1, recv(receiver.get(), &received, sizeof(received), 0));
  ASSERT_EQ(probe, received);

  const std::string table = "dkc035_mcast_" + std::to_string(getpid());
  constexpr uint32_t seq = 925;
  bool installed = false;
  [&] {
    auto batch = TableBatch(kNftNewtable, seq, table.c_str());
    batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
    auto chain = NewChain(seq + 2, table.c_str(), "prerouting", true, NF_DROP);
    batch.insert(batch.end(), chain.begin(), chain.end());
    auto end = Request(NFNL_MSG_BATCH_END, seq + 3);
    batch.insert(batch.end(), end.begin(), end.end());
    ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
    installed = true;
    Ack(nft.get(), seq + 1, 0, kNftNewtable);
    Ack(nft.get(), seq + 2, 0, kNftNewchain);

    const ssize_t sent = sendto(sender.get(), &probe, 1, 0,
                                reinterpret_cast<sockaddr*>(&address), sizeof(address));
    EXPECT_TRUE(sent == 1 || (sent == -1 && errno == EPERM));
    ready.revents = 0;
    EXPECT_EQ(0, poll(&ready, 1, 150));
  }();
  if (installed) {
    auto remove = TableBatch(kNftDeltable, seq + 4, table.c_str());
    ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
    Ack(nft.get(), seq + 5, 0, kNftDeltable);
  }
}

TEST(NetlinkNetfilter, NftForwardHookAndIpv4ForwardingSysctl) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd setting(open("/proc/sys/net/ipv4/ip_forward", O_RDWR));
  ASSERT_GE(setting.get(), 0) << std::strerror(errno);
  char original[32]{};
  const ssize_t original_len = read(setting.get(), original, sizeof(original));
  ASSERT_GT(original_len, 0);
  ASSERT_EQ(0, lseek(setting.get(), 0, SEEK_SET));
  ASSERT_EQ(2, write(setting.get(), "1\n", 2));
  ASSERT_EQ(0, lseek(setting.get(), 0, SEEK_SET));
  char enabled[32]{};
  const ssize_t enabled_len = read(setting.get(), enabled, sizeof(enabled));
  ASSERT_EQ(0, lseek(setting.get(), 0, SEEK_SET));
  const ssize_t restored = write(setting.get(), original, original_len);
  ASSERT_EQ(original_len, restored);
  ASSERT_GT(enabled_len, 0);
  EXPECT_EQ('1', enabled[0]);

  Fd nft(Open());
  ASSERT_GE(nft.get(), 0);
  constexpr uint32_t seq = 926;
  auto batch = TableBatch(kNftNewtable, seq, "dkc035_forward");
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto chain = NewChain(seq + 2, "dkc035_forward", "forward", true, NF_DROP,
                        NF_INET_FORWARD);
  batch.insert(batch.end(), chain.begin(), chain.end());
  auto end = Request(NFNL_MSG_BATCH_END, seq + 3);
  batch.insert(batch.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewchain);

  auto remove = TableBatch(kNftDeltable, seq + 4, "dkc035_forward");
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
  Ack(nft.get(), seq + 5, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, RawIngressPktinfoReportsActualVeth) {
  const unsigned injector_ifindex = if_nametoindex("veth1");
  const unsigned ingress_ifindex = if_nametoindex("veth2");
  ASSERT_NE(0u, injector_ifindex);
  ASSERT_NE(0u, ingress_ifindex);
  Fd raw(socket(AF_INET, SOCK_RAW | SOCK_NONBLOCK, IPPROTO_UDP));
  Fd injector(socket(AF_PACKET, SOCK_DGRAM, htons(ETH_P_IP)));
  ASSERT_GE(raw.get(), 0);
  ASSERT_GE(injector.get(), 0);
  const int one = 1;
  ASSERT_EQ(0, setsockopt(raw.get(), IPPROTO_IP, IP_PKTINFO, &one, sizeof(one)));

  const in_addr source{inet_addr("111.111.11.99")};
  const in_addr destination{inet_addr("111.111.11.2")};
  const auto packet = Ipv4UdpProbe(source.s_addr, destination.s_addr, 0x3501);
  ASSERT_TRUE(InjectVethIpProbe(injector.get(), injector_ifindex, "veth2", packet))
      << std::strerror(errno);
  pollfd ready{raw.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 2000));
  std::array<uint8_t, 128> received{};
  std::array<uint8_t, 128> controls{};
  iovec iov{received.data(), received.size()};
  msghdr message{};
  message.msg_iov = &iov;
  message.msg_iovlen = 1;
  message.msg_control = controls.data();
  message.msg_controllen = controls.size();
  const ssize_t count = recvmsg(raw.get(), &message, 0);
  ASSERT_GE(count, static_cast<ssize_t>(packet.size()));
  EXPECT_EQ(0, std::memcmp(received.data() + 12, &source.s_addr, 4));
  EXPECT_EQ(0, std::memcmp(received.data() + 16, &destination.s_addr, 4));
  bool found_pktinfo = false;
  for (cmsghdr* control = CMSG_FIRSTHDR(&message); control != nullptr;
       control = CMSG_NXTHDR(&message, control)) {
    if (control->cmsg_level != IPPROTO_IP || control->cmsg_type != IP_PKTINFO) continue;
    ASSERT_GE(control->cmsg_len, CMSG_LEN(sizeof(in_pktinfo)));
    in_pktinfo info{};
    std::memcpy(&info, CMSG_DATA(control), sizeof(info));
    EXPECT_EQ(static_cast<int>(ingress_ifindex), info.ipi_ifindex);
    found_pktinfo = true;
  }
  EXPECT_TRUE(found_pktinfo);
}

TEST(NetlinkNetfilter, PacketLegacyReceiveWakesEpoll) {
  const unsigned injector_ifindex = if_nametoindex("veth1");
  const unsigned ingress_ifindex = if_nametoindex("veth2");
  ASSERT_NE(0u, injector_ifindex);
  ASSERT_NE(0u, ingress_ifindex);
  Fd injector(socket(AF_PACKET, SOCK_DGRAM, htons(ETH_P_IP)));
  Fd observer(socket(AF_PACKET, SOCK_RAW | SOCK_NONBLOCK, htons(ETH_P_IP)));
  Fd epoll_fd(epoll_create1(EPOLL_CLOEXEC));
  ASSERT_GE(injector.get(), 0);
  ASSERT_GE(observer.get(), 0);
  ASSERT_GE(epoll_fd.get(), 0);
  sockaddr_ll address{};
  address.sll_family = AF_PACKET;
  address.sll_protocol = htons(ETH_P_IP);
  address.sll_ifindex = static_cast<int>(ingress_ifindex);
  ASSERT_EQ(0, bind(observer.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)));
  epoll_event interest{};
  interest.events = EPOLLIN;
  ASSERT_EQ(0, epoll_ctl(epoll_fd.get(), EPOLL_CTL_ADD, observer.get(), &interest));
  std::array<uint8_t, 128> frame{};
  while (recv(observer.get(), frame.data(), frame.size(), MSG_DONTWAIT) > 0) {
  }

  const in_addr source{inet_addr("111.111.11.99")};
  const in_addr destination{inet_addr("111.111.11.2")};
  const pid_t child = fork();
  ASSERT_GE(child, 0);
  if (child == 0) {
    usleep(50000);
    const bool sent = InjectVethIpProbe(injector.get(), injector_ifindex, "veth2",
                                        Ipv4UdpProbe(source.s_addr, destination.s_addr, 0x35e1));
    _exit(sent ? 0 : 1);
  }
  bool received_probe = false;
  const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(2);
  while (!received_probe && std::chrono::steady_clock::now() < deadline) {
    const auto remaining = std::chrono::duration_cast<std::chrono::milliseconds>(
        deadline - std::chrono::steady_clock::now());
    epoll_event ready{};
    const int events = epoll_wait(epoll_fd.get(), &ready, 1,
                                  static_cast<int>(remaining.count()));
    if (events < 0 && errno == EINTR) continue;
    if (events != 1 || !(ready.events & EPOLLIN)) break;
    sockaddr_ll sender{};
    socklen_t sender_len = sizeof(sender);
    for (;;) {
      const ssize_t count = recvfrom(observer.get(), frame.data(), frame.size(), MSG_DONTWAIT,
                                     reinterpret_cast<sockaddr*>(&sender), &sender_len);
      if (count < 0) break;
      if (count >= 42 && sender.sll_ifindex == static_cast<int>(ingress_ifindex) &&
          frame[18] == 0x35 && frame[19] == 0xe1) {
        received_probe = true;
        break;
      }
      sender_len = sizeof(sender);
    }
  }
  int child_status = 0;
  ASSERT_EQ(child, waitpid(child, &child_status, 0));
  ASSERT_TRUE(WIFEXITED(child_status));
  ASSERT_EQ(0, WEXITSTATUS(child_status));
  EXPECT_TRUE(received_probe) << "queued AF_PACKET data must wake epoll";
}

TEST(NetlinkNetfilter, NftForwardDropGatesActualRoutedIngress) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  const unsigned injector_ifindex = if_nametoindex("veth1");
  const unsigned ingress_ifindex = if_nametoindex("veth2");
  ASSERT_NE(0u, injector_ifindex);
  ASSERT_NE(0u, ingress_ifindex);
  Fd injector(socket(AF_PACKET, SOCK_DGRAM, htons(ETH_P_IP)));
  Fd observer(socket(AF_PACKET, SOCK_RAW, htons(ETH_P_ARP)));
  Fd setting(open("/proc/sys/net/ipv4/ip_forward", O_RDWR));
  FdGuard rtnl(OpenRouteSocket());
  Fd nft(Open());
  ASSERT_GE(injector.get(), 0);
  ASSERT_GE(observer.get(), 0);
  ASSERT_GE(setting.get(), 0);
  ASSERT_GE(rtnl.Get(), 0);
  ASSERT_GE(nft.get(), 0);
  sockaddr_ll bind_address{};
  bind_address.sll_family = AF_PACKET;
  bind_address.sll_protocol = htons(ETH_P_ARP);
  bind_address.sll_ifindex = static_cast<int>(injector_ifindex);
  ASSERT_EQ(0, bind(observer.get(), reinterpret_cast<sockaddr*>(&bind_address),
                    sizeof(bind_address)));

  char original[32]{};
  const ssize_t original_len = read(setting.get(), original, sizeof(original));
  ASSERT_GT(original_len, 0);
  auto set_forwarding = [&](const char* value, size_t length) {
    if (lseek(setting.get(), 0, SEEK_SET) != 0) return false;
    return write(setting.get(), value, length) == static_cast<ssize_t>(length);
  };
  uint32_t route_seq = 12000;
  RouteSpec route = MakeIpv4Route("198.18.250.99", 32, ingress_ifindex);
  bool route_added = false;
  bool table_maybe_added = false;
  const std::string table_name = "dkc035_forward_gate_" + std::to_string(getpid());
  const in_addr source{inet_addr("111.111.11.99")};
  const in_addr destination{inet_addr("198.18.250.99")};
  auto inject = [&](uint16_t id) {
    return InjectVethIpProbe(injector.get(), injector_ifindex, "veth2",
                             Ipv4UdpProbe(source.s_addr, destination.s_addr, id));
  };
  constexpr uint32_t seq = 930;
  // ASSERT_* returns only from this lambda; cleanup below always runs.
  auto exercise = [&] {
    ASSERT_TRUE(set_forwarding("0\n", 2));
    ASSERT_EQ(0, SendRouteRequest(rtnl.Get(), RTM_NEWROUTE,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                  route, ++route_seq));
    route_added = true;
    ASSERT_TRUE(inject(0x3502));
    EXPECT_FALSE(SawArpTarget(observer.get(), destination.s_addr, 150))
        << "IPv4 forwarding must start disabled";
    ASSERT_TRUE(set_forwarding("1\n", 2));

    auto batch = TableBatch(kNftNewtable, seq, table_name.c_str());
    batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
    auto chain = NewChain(seq + 2, table_name.c_str(), "forward", true, NF_ACCEPT,
                          NF_INET_FORWARD);
    batch.insert(batch.end(), chain.begin(), chain.end());
    // Linux decrements the IPv4 TTL before the FORWARD hook. A rule for the
    // original TTL (64) must not match this forwarded datagram; the rule for
    // 63 must drop it before neighbor discovery.
    auto ttl_rule = ProtocolCounterRule(
        seq + 3, table_name.c_str(), "forward", NFT_CMP_EQ,
        NFT_PAYLOAD_NETWORK_HEADER, NFT_REG_1, 8, UINT32_MAX, false, 1,
        true, nullptr, 63);
    batch.insert(batch.end(), ttl_rule.begin(), ttl_rule.end());
    auto end = Request(NFNL_MSG_BATCH_END, seq + 4);
    batch.insert(batch.end(), end.begin(), end.end());
    ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
    table_maybe_added = true;
    Ack(nft.get(), seq + 1, 0, kNftNewtable);
    Ack(nft.get(), seq + 2, 0, kNftNewchain);
    Ack(nft.get(), seq + 3, 0, kNftNewrule);
    ASSERT_TRUE(inject(0x3503));
    EXPECT_FALSE(SawArpTarget(observer.get(), destination.s_addr, 150))
        << "FORWARD DROP must run before neighbor resolution";

    auto remove = TableBatch(kNftDeltable, seq + 5, table_name.c_str());
    ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
    Ack(nft.get(), seq + 6, 0, kNftDeltable);
    table_maybe_added = false;
    ASSERT_TRUE(inject(0x3504));
    EXPECT_TRUE(SawArpTarget(observer.get(), destination.s_addr, 2000))
        << "forwarding enabled without DROP must reach the egress neighbor";
  };
  exercise();
  if (table_maybe_added) {
    auto remove = TableBatch(kNftDeltable, seq + 7, table_name.c_str());
    if (Send(nft.get(), remove) == static_cast<ssize_t>(remove.size())) {
      Ack(nft.get(), seq + 8, 0, kNftDeltable);
    }
  }
  if (route_added) {
    EXPECT_EQ(0, SendRouteRequest(rtnl.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK,
                                  route, ++route_seq));
  }
  EXPECT_TRUE(set_forwarding(original, static_cast<size_t>(original_len)));
}

TEST(NetlinkNetfilter, StatelessIpv6LocalInputCountsFragmentsBeforeReassembly) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd nft(Open());
  Fd receiver(socket(AF_INET6, SOCK_DGRAM, 0));
  Fd sender(socket(AF_INET6, SOCK_DGRAM, 0));
  ASSERT_GE(nft.get(), 0);
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(sender.get(), 0);
  sockaddr_in6 local{};
  local.sin6_family = AF_INET6;
  local.sin6_addr = in6addr_loopback;
  ASSERT_EQ(0, bind(receiver.get(), reinterpret_cast<sockaddr*>(&local), sizeof(local)));
  socklen_t local_len = sizeof(local);
  ASSERT_EQ(0, getsockname(receiver.get(), reinterpret_cast<sockaddr*>(&local), &local_len));

  const std::string table = "dkc035_frag_input_" + std::to_string(getpid());
  constexpr uint32_t seq = 969;
  auto batch = TableBatch(kNftNewtable, seq, table.c_str(), NFPROTO_IPV6);
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto chain = NewChain(seq + 2, table.c_str(), "input", true, NF_ACCEPT,
                        NF_INET_LOCAL_IN, NFPROTO_IPV6);
  auto rule = ProtocolCounterRule(seq + 3, table.c_str(), "input", NFT_CMP_EQ,
                                  NFT_PAYLOAD_NETWORK_HEADER, NFT_REG_1, 6,
                                  UINT32_MAX, false, 1, false, nullptr, 44,
                                  NFPROTO_IPV6);
  batch.insert(batch.end(), chain.begin(), chain.end());
  batch.insert(batch.end(), rule.begin(), rule.end());
  auto end = Request(NFNL_MSG_BATCH_END, seq + 4);
  batch.insert(batch.end(), end.begin(), end.end());
  bool installed = false;
  [&] {
    ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
    installed = true;
    Ack(nft.get(), seq + 1, 0, kNftNewtable);
    Ack(nft.get(), seq + 2, 0, kNftNewchain);
    Ack(nft.get(), seq + 3, 0, kNftNewrule);
    std::vector<uint8_t> payload(65527, 0x35);
    ASSERT_EQ(static_cast<ssize_t>(payload.size()),
              sendto(sender.get(), payload.data(), payload.size(), 0,
                     reinterpret_cast<sockaddr*>(&local), sizeof(local)));
    pollfd ready{receiver.get(), POLLIN, 0};
    ASSERT_EQ(1, poll(&ready, 1, 3000));
    std::vector<uint8_t> received(payload.size());
    ASSERT_EQ(static_cast<ssize_t>(payload.size()),
              recv(receiver.get(), received.data(), received.size(), 0));
    EXPECT_EQ(payload, received);
    auto query = Request(kNftGetrule, seq + 5);
    query[NLMSG_HDRLEN] = NFPROTO_IPV6;
    nlmsghdr header{};
    std::memcpy(&header, query.data(), sizeof(header));
    header.nlmsg_flags |= NLM_F_DUMP;
    std::memcpy(query.data(), &header, sizeof(header));
    AppendAttr(&query, NFTA_RULE_TABLE, table.c_str());
    AppendAttr(&query, NFTA_RULE_CHAIN, "input");
    ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
    const auto rules = ReceiveRuleDump(nft.get(), header.nlmsg_seq);
    ASSERT_EQ(1u, rules.size());
    EXPECT_GE(rules[0].packets, 2u)
        << "stateless LOCAL_IN must see each IPv6 fragment before reassembly";
  }();
  if (installed) {
    auto remove = TableBatch(kNftDeltable, seq + 6, table.c_str(), NFPROTO_IPV6);
    ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
    Ack(nft.get(), seq + 7, 0, kNftDeltable);
  }
}

TEST(NetlinkNetfilter, NftIpv6AndInetForwardSeeDecrementedHopAndRealDevices) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  const unsigned injector_ifindex = if_nametoindex("veth1");
  const unsigned ingress_ifindex = if_nametoindex("veth2");
  ASSERT_NE(0u, injector_ifindex);
  ASSERT_NE(0u, ingress_ifindex);
  Fd injector(socket(AF_PACKET, SOCK_DGRAM, htons(ETH_P_IPV6)));
  Fd observer(socket(AF_PACKET, SOCK_RAW, htons(ETH_P_IPV6)));
  Fd setting(open("/proc/sys/net/ipv6/conf/all/forwarding", O_RDWR));
  FdGuard rtnl(OpenRouteSocket());
  Fd nft(Open());
  ASSERT_GE(injector.get(), 0);
  ASSERT_GE(observer.get(), 0);
  ASSERT_GE(setting.get(), 0);
  ASSERT_GE(rtnl.Get(), 0);
  ASSERT_GE(nft.get(), 0);
  sockaddr_ll bind_address{};
  bind_address.sll_family = AF_PACKET;
  bind_address.sll_protocol = htons(ETH_P_IPV6);
  bind_address.sll_ifindex = static_cast<int>(ingress_ifindex);
  ASSERT_EQ(0, bind(observer.get(), reinterpret_cast<sockaddr*>(&bind_address),
                    sizeof(bind_address)));
  char original[32]{};
  const ssize_t original_len = read(setting.get(), original, sizeof(original));
  ASSERT_GT(original_len, 0);
  auto set_forwarding = [&](const char* value) {
    if (lseek(setting.get(), 0, SEEK_SET) != 0) return false;
    // Linux proc_dointvec accepts the number but reports a consumed byte
    // before the trailing newline; DragonOS may report the full input.
    const ssize_t written = write(setting.get(), value, std::strlen(value));
    return written == static_cast<ssize_t>(std::strlen(value)) ||
           (written > 0 && value[written] == '\n');
  };
  constexpr const char* kDestination = "fd10:35::99";
  uint32_t route_seq = 13100;
  bool address_added = false, route_added = false;
  bool ip6_table_added = false, inet_table_added = false;
  constexpr uint32_t seq = 950;
  const std::string ip6_table = "dkc035_ip6_forward_" + std::to_string(getpid());
  const std::string inet_table = "dkc035_inet_forward_" + std::to_string(getpid());
  auto inject = [&] {
    return InjectVethIpProbe(injector.get(), injector_ifindex, "veth2",
                             Ipv6NoNextProbe("fd10:35::2", kDestination), ETH_P_IPV6);
  };
  auto install = [&](uint8_t family, const std::string& table, int32_t priority,
                     bool drop) {
    auto batch = TableBatch(kNftNewtable, seq, table.c_str(), family);
    batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
    auto chain = NewChain(seq + 2, table.c_str(), "forward", true, NF_ACCEPT,
                          NF_INET_FORWARD, family, priority);
    batch.insert(batch.end(), chain.begin(), chain.end());
    std::array<uint8_t, IFNAMSIZ> iifname{}, oifname{};
    std::memcpy(iifname.data(), "veth2", 5);
    std::memcpy(oifname.data(), "veth1", 5);
    auto iif = MetaCounterRule(seq + 3, table.c_str(), "forward", NFT_META_IIFNAME,
                               iifname.data(), iifname.size(), family);
    auto oif = MetaCounterRule(seq + 4, table.c_str(), "forward", NFT_META_OIFNAME,
                               oifname.data(), oifname.size(), family);
    auto hop = ProtocolCounterRule(seq + 5, table.c_str(), "forward", NFT_CMP_EQ,
                                   NFT_PAYLOAD_NETWORK_HEADER, NFT_REG_1, 7,
                                   UINT32_MAX, false, 1, drop, nullptr, 63, family);
    for (const auto* message : {&iif, &oif, &hop})
      batch.insert(batch.end(), message->begin(), message->end());
    auto end = Request(NFNL_MSG_BATCH_END, seq + 6);
    batch.insert(batch.end(), end.begin(), end.end());
    if (Send(nft.get(), batch) != static_cast<ssize_t>(batch.size())) return false;
    Ack(nft.get(), seq + 1, 0, kNftNewtable);
    Ack(nft.get(), seq + 2, 0, kNftNewchain);
    Ack(nft.get(), seq + 3, 0, kNftNewrule);
    Ack(nft.get(), seq + 4, 0, kNftNewrule);
    Ack(nft.get(), seq + 5, 0, kNftNewrule);
    return !testing::Test::HasFailure();
  };
  auto exercise = [&] {
    ASSERT_TRUE(set_forwarding("0\n"));
    ASSERT_EQ(0, SendIpv6AddrRequest(rtnl.Get(), RTM_NEWADDR,
                                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                      injector_ifindex, "fd10:35::1", 64, ++route_seq));
    address_added = true;
    ASSERT_EQ(0, SendIpv6RouteRequest(rtnl.Get(), RTM_NEWROUTE,
                                       NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                       kDestination, 128, injector_ifindex,
                                       RT_SCOPE_LINK, ++route_seq));
    route_added = true;
    ASSERT_TRUE(inject());
    EXPECT_FALSE(SawNeighborSolicitTarget(observer.get(), kDestination, 150))
        << "IPv6 forwarding must start disabled";
    ASSERT_TRUE(set_forwarding("1\n"));
    ASSERT_TRUE(install(NFPROTO_INET, inet_table, -10, false));
    inet_table_added = true;
    ASSERT_TRUE(install(NFPROTO_IPV6, ip6_table, 0, true));
    ip6_table_added = true;
    ASSERT_TRUE(inject());
    EXPECT_FALSE(SawNeighborSolicitTarget(observer.get(), kDestination, 150))
        << "IPv6 FORWARD DROP must run before neighbor discovery";
    for (const auto& [family, table] : {
             std::pair<uint8_t, const char*>{NFPROTO_INET, inet_table.c_str()},
             {NFPROTO_IPV6, ip6_table.c_str()}}) {
      auto query = Request(kNftGetrule, ++route_seq);
      query[NLMSG_HDRLEN] = family;
      nlmsghdr header{};
      std::memcpy(&header, query.data(), sizeof(header));
      header.nlmsg_flags |= NLM_F_DUMP;
      std::memcpy(query.data(), &header, sizeof(header));
      AppendAttr(&query, NFTA_RULE_TABLE, table);
      AppendAttr(&query, NFTA_RULE_CHAIN, "forward");
      ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
      const auto rules = ReceiveRuleDump(nft.get(), header.nlmsg_seq);
      ASSERT_EQ(3u, rules.size());
      for (const auto& rule : rules) {
        EXPECT_TRUE(rule.has_counter);
        EXPECT_EQ(1u, rule.packets);
      }
    }
    for (const auto& [family, table] : {
             std::pair<uint8_t, const char*>{NFPROTO_IPV6, ip6_table.c_str()},
             {NFPROTO_INET, inet_table.c_str()}}) {
      auto remove = TableBatch(kNftDeltable, ++route_seq, table, family);
      ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
      Ack(nft.get(), route_seq + 1, 0, kNftDeltable);
      if (family == NFPROTO_IPV6) ip6_table_added = false;
      else inet_table_added = false;
      route_seq += 2;
    }
    ASSERT_TRUE(inject());
    EXPECT_TRUE(SawNeighborSolicitTarget(observer.get(), kDestination, 5000))
        << "without a DROP rule, routed IPv6 must reach egress neighbor discovery";
  };
  exercise();
  for (const auto& [family, table, added] : {
           std::tuple<uint8_t, const char*, bool>{NFPROTO_IPV6, ip6_table.c_str(), ip6_table_added},
           {NFPROTO_INET, inet_table.c_str(), inet_table_added}}) {
    if (!added) continue;
    auto remove = TableBatch(kNftDeltable, ++route_seq, table, family);
    if (Send(nft.get(), remove) == static_cast<ssize_t>(remove.size()))
      Ack(nft.get(), route_seq + 1, 0, kNftDeltable);
    route_seq += 2;
  }
  if (route_added) {
    EXPECT_EQ(0, SendIpv6RouteRequest(rtnl.Get(), RTM_DELROUTE,
                                       NLM_F_REQUEST | NLM_F_ACK, kDestination, 128,
                                       injector_ifindex, RT_SCOPE_LINK, ++route_seq));
  }
  if (address_added) {
    EXPECT_EQ(0, SendIpv6AddrRequest(rtnl.Get(), RTM_DELADDR,
                                      NLM_F_REQUEST | NLM_F_ACK, injector_ifindex,
                                      "fd10:35::1", 64, ++route_seq));
  }
  EXPECT_TRUE(set_forwarding(original));
}

TEST(NetlinkNetfilter, StatelessIpv6TransitFragmentDoesNotWaitForReassembly) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  // Linux hosts with nf_conntrack loaded may register early IPv6 defrag for
  // the whole network namespace, including this otherwise empty ruleset.
  // This test specifically exercises the no-conntrack forwarding path.
  if (access("/proc/sys/net/netfilter/nf_conntrack_count", F_OK) == 0)
    GTEST_SKIP() << "host nf_conntrack may enable PRE_ROUTING defrag";
  const unsigned injector_ifindex = if_nametoindex("veth1");
  const unsigned ingress_ifindex = if_nametoindex("veth2");
  ASSERT_NE(0u, injector_ifindex);
  ASSERT_NE(0u, ingress_ifindex);
  Fd injector(socket(AF_PACKET, SOCK_DGRAM, htons(ETH_P_IPV6)));
  Fd observer(socket(AF_PACKET, SOCK_RAW, htons(ETH_P_IPV6)));
  Fd setting(open("/proc/sys/net/ipv6/conf/all/forwarding", O_RDWR));
  FdGuard rtnl(OpenRouteSocket());
  ASSERT_GE(injector.get(), 0);
  ASSERT_GE(observer.get(), 0);
  ASSERT_GE(setting.get(), 0);
  ASSERT_GE(rtnl.Get(), 0);
  sockaddr_ll bind_address{};
  bind_address.sll_family = AF_PACKET;
  bind_address.sll_protocol = htons(ETH_P_IPV6);
  bind_address.sll_ifindex = static_cast<int>(ingress_ifindex);
  ASSERT_EQ(0, bind(observer.get(), reinterpret_cast<sockaddr*>(&bind_address),
                    sizeof(bind_address)));
  char original[32]{};
  const ssize_t original_len = read(setting.get(), original, sizeof(original));
  ASSERT_GT(original_len, 0);
  auto set_forwarding = [&](const char* value, size_t length) {
    if (lseek(setting.get(), 0, SEEK_SET) != 0) return false;
    const ssize_t written = write(setting.get(), value, length);
    return written == static_cast<ssize_t>(length) ||
           (written > 0 && value[written] == '\n');
  };
  constexpr const char* kDestination = "fd10:35::99";
  uint32_t route_seq = 13900;
  bool address_added = false, route_added = false;
  [&] {
    ASSERT_TRUE(set_forwarding("1\n", 2));
    ASSERT_EQ(0, SendIpv6AddrRequest(rtnl.Get(), RTM_NEWADDR,
                                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                      injector_ifindex, "fd10:35::1", 64, ++route_seq));
    address_added = true;
    ASSERT_EQ(0, SendIpv6RouteRequest(rtnl.Get(), RTM_NEWROUTE,
                                       NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                       kDestination, 128, injector_ifindex,
                                       RT_SCOPE_LINK, ++route_seq));
    route_added = true;
    const auto fragment = Ipv6FirstFragmentProbe("fd10:35::2", kDestination);
    ASSERT_TRUE(InjectVethIpProbe(injector.get(), injector_ifindex, "veth2",
                                  fragment, ETH_P_IPV6));
    EXPECT_TRUE(SawNeighborSolicitTarget(observer.get(), kDestination, 2000))
        << "a stateless transit fragment must route without waiting for its tail";
  }();
  if (route_added) {
    EXPECT_EQ(0, SendIpv6RouteRequest(rtnl.Get(), RTM_DELROUTE,
                                       NLM_F_REQUEST | NLM_F_ACK, kDestination, 128,
                                       injector_ifindex, RT_SCOPE_LINK, ++route_seq));
  }
  if (address_added) {
    EXPECT_EQ(0, SendIpv6AddrRequest(rtnl.Get(), RTM_DELADDR,
                                      NLM_F_REQUEST | NLM_F_ACK, injector_ifindex,
                                      "fd10:35::1", 64, ++route_seq));
  }
  EXPECT_TRUE(set_forwarding(original, static_cast<size_t>(original_len)));
}

// The sendto route, not the IP_HDRINCL header daddr, chooses loopback. Raw
// receive must see the original TOS/DF/daddr instead of a re-emitted IpRepr.
TEST(NetlinkNetfilter, RawHdrinclLoopbackPreservesOriginalHeader) {
  Fd receiver(socket(AF_INET, SOCK_RAW | SOCK_NONBLOCK, IPPROTO_UDP));
  Fd sender(socket(AF_INET, SOCK_RAW, IPPROTO_RAW));
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(sender.get(), 0);

  sockaddr_in address{};
  address.sin_family = AF_INET;
  address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  std::array<uint8_t, 28> packet{};
  packet[0] = 0x45;
  packet[1] = 0x2e;  // TOS
  packet[6] = 0x40;  // DF
  packet[8] = 64;    // TTL
  packet[9] = IPPROTO_UDP;
  packet[16] = 192;
  packet[17] = 0;
  packet[18] = 2;
  packet[19] = 1;
  packet[20] = 0x12;
  packet[21] = 0x34;
  packet[22] = 0x56;
  packet[23] = 0x78;
  packet[25] = 8;  // UDP length

  ASSERT_EQ(static_cast<ssize_t>(packet.size()),
            sendto(sender.get(), packet.data(), packet.size(), 0,
                   reinterpret_cast<sockaddr*>(&address), sizeof(address)));
  pollfd ready{receiver.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 2000));
  std::array<uint8_t, 64> received{};
  ASSERT_EQ(static_cast<ssize_t>(packet.size()),
            recv(receiver.get(), received.data(), received.size(), 0));
  EXPECT_EQ(0x2e, received[1]);
  EXPECT_EQ(0x40, received[6] & 0x40);
  EXPECT_EQ(192, received[16]);
  EXPECT_EQ(0, received[17]);
  EXPECT_EQ(2, received[18]);
  EXPECT_EQ(1, received[19]);
  EXPECT_EQ(127, received[12]);
  EXPECT_EQ(0, received[13]);
  EXPECT_EQ(0, received[14]);
  EXPECT_EQ(1, received[15]);
}

TEST(NetlinkNetfilter, RawBoundBroadcastUsesOutputDeviceSource) {
  Fd receiver(socket(AF_INET, SOCK_RAW | SOCK_NONBLOCK, IPPROTO_UDP));
  Fd sender(socket(AF_INET, SOCK_RAW, IPPROTO_UDP));
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(sender.get(), 0);
  sockaddr_in local{};
  local.sin_family = AF_INET;
  local.sin_addr.s_addr = htonl(INADDR_BROADCAST);
  ASSERT_EQ(0, bind(sender.get(), reinterpret_cast<sockaddr*>(&local), sizeof(local)));
  sockaddr_in destination{};
  destination.sin_family = AF_INET;
  destination.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  std::array<uint8_t, 8> udp_header{};
  udp_header[5] = 8;
  ASSERT_EQ(static_cast<ssize_t>(udp_header.size()),
            sendto(sender.get(), udp_header.data(), udp_header.size(), 0,
                   reinterpret_cast<sockaddr*>(&destination), sizeof(destination)));
  pollfd ready{receiver.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 2000));
  std::array<uint8_t, 128> packet{};
  ASSERT_GE(recv(receiver.get(), packet.data(), packet.size(), 0), 28);
  EXPECT_EQ(127, packet[12]);
  EXPECT_EQ(0, packet[13]);
  EXPECT_EQ(0, packet[14]);
  EXPECT_EQ(1, packet[15]);
}

TEST(NetlinkNetfilter, RawLoopbackSourceCannotLeavePhysicalOutput) {
  ASSERT_NE(0u, if_nametoindex("veth1"));
  Fd sender(socket(AF_INET, SOCK_RAW, IPPROTO_UDP));
  ASSERT_GE(sender.get(), 0);
  sockaddr_in local{};
  local.sin_family = AF_INET;
  local.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  ASSERT_EQ(0, bind(sender.get(), reinterpret_cast<sockaddr*>(&local), sizeof(local)));
  sockaddr_in destination{};
  destination.sin_family = AF_INET;
  ASSERT_EQ(1, inet_pton(AF_INET, "111.111.11.253", &destination.sin_addr));
  std::array<uint8_t, 8> udp_header{};
  udp_header[5] = 8;
  EXPECT_EQ(-1, sendto(sender.get(), udp_header.data(), udp_header.size(), 0,
                       reinterpret_cast<sockaddr*>(&destination), sizeof(destination)));
  EXPECT_EQ(EINVAL, errno);
}

TEST(NetlinkNetfilter, RawMulticastSendmsgTtlOverridesSocketDefault) {
  Fd receiver(socket(AF_INET, SOCK_RAW | SOCK_NONBLOCK, IPPROTO_UDP));
  Fd sender(socket(AF_INET, SOCK_RAW, IPPROTO_UDP));
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(sender.get(), 0);
  const unsigned loopback = if_nametoindex("lo");
  ASSERT_NE(0u, loopback);
  sockaddr_in destination{};
  destination.sin_family = AF_INET;
  ASSERT_EQ(1, inet_pton(AF_INET, "239.255.35.12", &destination.sin_addr));
  ip_mreqn membership{};
  membership.imr_multiaddr = destination.sin_addr;
  membership.imr_ifindex = static_cast<int>(loopback);
  ASSERT_EQ(0, setsockopt(receiver.get(), IPPROTO_IP, IP_ADD_MEMBERSHIP,
                          &membership, sizeof(membership)));
  ASSERT_EQ(0, setsockopt(sender.get(), IPPROTO_IP, IP_MULTICAST_IF,
                          &membership, sizeof(membership)));
  const int default_ttl = 1;
  ASSERT_EQ(0, setsockopt(sender.get(), IPPROTO_IP, IP_MULTICAST_TTL,
                          &default_ttl, sizeof(default_ttl)));

  std::array<uint8_t, 8> udp_header{};
  udp_header[5] = 8;
  iovec iov{udp_header.data(), udp_header.size()};
  std::array<uint8_t, CMSG_SPACE(sizeof(int))> control{};
  msghdr message{};
  message.msg_name = &destination;
  message.msg_namelen = sizeof(destination);
  message.msg_iov = &iov;
  message.msg_iovlen = 1;
  message.msg_control = control.data();
  message.msg_controllen = control.size();
  cmsghdr* cmsg = CMSG_FIRSTHDR(&message);
  ASSERT_NE(nullptr, cmsg);
  cmsg->cmsg_level = IPPROTO_IP;
  cmsg->cmsg_type = IP_TTL;
  cmsg->cmsg_len = CMSG_LEN(sizeof(int));
  const int override_ttl = 5;
  std::memcpy(CMSG_DATA(cmsg), &override_ttl, sizeof(override_ttl));
  ASSERT_EQ(static_cast<ssize_t>(udp_header.size()), sendmsg(sender.get(), &message, 0));
  pollfd ready{receiver.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 2000));
  std::array<uint8_t, 128> packet{};
  ASSERT_GE(recv(receiver.get(), packet.data(), packet.size(), 0), 28);
  EXPECT_EQ(override_ttl, packet[8]);
}

// A rule is successful only if the packet path executes it. Build the entire
// transaction at once to also cover references to objects created in-batch.
TEST(NetlinkNetfilter, NftImmediateRuleOverridesBasePolicy) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd receiver(socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0));
  Fd sender(socket(AF_INET, SOCK_DGRAM, 0));
  Fd nft(Open());
  ASSERT_GE(receiver.get(), 0);
  ASSERT_GE(sender.get(), 0);
  ASSERT_GE(nft.get(), 0);
  sockaddr_in address{};
  address.sin_family = AF_INET;
  address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  ASSERT_EQ(0, bind(receiver.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)));
  socklen_t address_len = sizeof(address);
  ASSERT_EQ(0, getsockname(receiver.get(), reinterpret_cast<sockaddr*>(&address), &address_len));

  constexpr uint32_t seq = 1050;
  auto begin = Request(NFNL_MSG_BATCH_BEGIN, seq);
  nfgenmsg begin_gen{};
  begin_gen.res_id = htons(NFNL_SUBSYS_NFTABLES);
  std::memcpy(begin.data() + NLMSG_HDRLEN, &begin_gen, sizeof(begin_gen));
  auto table = Request(kNftNewtable, seq + 1);
  table[NLMSG_HDRLEN] = NFPROTO_IPV4;
  nlmsghdr header{};
  std::memcpy(&header, table.data(), sizeof(header));
  header.nlmsg_flags |= NLM_F_CREATE | NLM_F_EXCL;
  std::memcpy(table.data(), &header, sizeof(header));
  AppendAttr(&table, NFTA_TABLE_NAME, "dkc035_rule");

  auto chain = Request(kNftNewchain, seq + 2);
  chain[NLMSG_HDRLEN] = NFPROTO_IPV4;
  std::memcpy(&header, chain.data(), sizeof(header));
  header.nlmsg_flags |= NLM_F_CREATE | NLM_F_EXCL;
  std::memcpy(chain.data(), &header, sizeof(header));
  AppendAttr(&chain, NFTA_CHAIN_TABLE, "dkc035_rule");
  AppendAttr(&chain, NFTA_CHAIN_NAME, "prerouting");
  AppendAttr(&chain, NFTA_CHAIN_TYPE, "filter");
  std::vector<uint8_t> hook;
  const uint32_t hooknum = htonl(NF_INET_PRE_ROUTING);
  const uint32_t priority = htonl(0);
  AppendNla(&hook, NFTA_HOOK_HOOKNUM,
            reinterpret_cast<const uint8_t*>(&hooknum), sizeof(hooknum));
  AppendNla(&hook, NFTA_HOOK_PRIORITY,
            reinterpret_cast<const uint8_t*>(&priority), sizeof(priority));
  AppendRawAttr(&chain, NFTA_CHAIN_HOOK | NLA_F_NESTED, hook.data(), hook.size());
  AppendBe32(&chain, NFTA_CHAIN_POLICY, NF_DROP);

  auto rule = ImmediateRule(seq + 3, "dkc035_rule", "prerouting", NF_ACCEPT, true);

  auto end = Request(NFNL_MSG_BATCH_END, seq + 4);
  begin.insert(begin.end(), table.begin(), table.end());
  begin.insert(begin.end(), chain.begin(), chain.end());
  begin.insert(begin.end(), rule.begin(), rule.end());
  begin.insert(begin.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(begin.size()), Send(nft.get(), begin));
  Ack(nft.get(), seq + 1, 0, kNftNewtable);
  Ack(nft.get(), seq + 2, 0, kNftNewchain);
  Ack(nft.get(), seq + 3, 0, kNftNewrule);

  const uint8_t probe = 7;
  ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                      reinterpret_cast<sockaddr*>(&address), address_len));
  pollfd ready{receiver.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&ready, 1, 2000));
  uint8_t received = 0;
  ASSERT_EQ(1, recv(receiver.get(), &received, 1, 0));
  EXPECT_EQ(probe, received);

  auto query = Request(kNftGetrule, seq + 5);
  query[NLMSG_HDRLEN] = NFPROTO_IPV4;
  std::memcpy(&header, query.data(), sizeof(header));
  header.nlmsg_flags |= NLM_F_DUMP;
  std::memcpy(query.data(), &header, sizeof(header));
  AppendAttr(&query, NFTA_RULE_TABLE, "dkc035_rule");
  AppendAttr(&query, NFTA_RULE_CHAIN, "prerouting");
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
  const auto initial_rules = ReceiveRuleDump(nft.get(), seq + 5);
  ASSERT_EQ(1u, initial_rules.size());
  ASSERT_NE(0u, initial_rules[0].handle);
  EXPECT_NE(0, initial_rules[0].flags & NLM_F_APPEND);

  auto mutate = [&](const std::vector<uint8_t>& change, uint32_t batch_seq,
                    uint16_t kind) {
    auto batch = Request(NFNL_MSG_BATCH_BEGIN, batch_seq);
    nfgenmsg gen{};
    gen.res_id = htons(NFNL_SUBSYS_NFTABLES);
    std::memcpy(batch.data() + NLMSG_HDRLEN, &gen, sizeof(gen));
    auto end = Request(NFNL_MSG_BATCH_END, batch_seq + 2);
    batch.insert(batch.end(), change.begin(), change.end());
    batch.insert(batch.end(), end.begin(), end.end());
    EXPECT_EQ(static_cast<ssize_t>(batch.size()), Send(nft.get(), batch));
    Ack(nft.get(), batch_seq + 1, 0, kind);
  };

  mutate(ImmediateRule(seq + 21, "dkc035_rule", "prerouting", NF_DROP, true),
         seq + 20, kNftNewrule);
  ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                      reinterpret_cast<sockaddr*>(&address), address_len));
  ready.revents = 0;
  ASSERT_EQ(1, poll(&ready, 1, 2000));
  ASSERT_EQ(1, recv(receiver.get(), &received, 1, 0));

  std::memcpy(&header, query.data(), sizeof(header));
  header.nlmsg_seq = seq + 23;
  std::memcpy(query.data(), &header, sizeof(header));
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(nft.get(), query));
  const auto appended_rules = ReceiveRuleDump(nft.get(), seq + 23);
  ASSERT_EQ(2u, appended_rules.size());
  EXPECT_EQ(initial_rules[0].handle, appended_rules[0].handle);
  EXPECT_EQ(initial_rules[0].handle, appended_rules[1].position);
  EXPECT_NE(0, appended_rules[1].flags & NLM_F_APPEND);

  mutate(ImmediateRule(seq + 25, "dkc035_rule", "prerouting", NF_DROP, false),
         seq + 24, kNftNewrule);
  ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                      reinterpret_cast<sockaddr*>(&address), address_len));
  ready.revents = 0;
  EXPECT_EQ(0, poll(&ready, 1, 150));

  auto flush_chain = Request(kNftDelrule, seq + 27);
  flush_chain[NLMSG_HDRLEN] = NFPROTO_IPV4;
  AppendAttr(&flush_chain, NFTA_RULE_TABLE, "dkc035_rule");
  AppendAttr(&flush_chain, NFTA_RULE_CHAIN, "prerouting");
  mutate(flush_chain, seq + 26, kNftDelrule);
  ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                      reinterpret_cast<sockaddr*>(&address), address_len));
  ready.revents = 0;
  EXPECT_EQ(0, poll(&ready, 1, 150));

  std::memcpy(&header, rule.data(), sizeof(header));
  header.nlmsg_seq = seq + 30;
  std::memcpy(rule.data(), &header, sizeof(header));
  mutate(rule, seq + 29, kNftNewrule);
  ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                      reinterpret_cast<sockaddr*>(&address), address_len));
  ready.revents = 0;
  ASSERT_EQ(1, poll(&ready, 1, 2000));
  ASSERT_EQ(1, recv(receiver.get(), &received, 1, 0));

  auto flush_table = Request(kNftDelrule, seq + 33);
  flush_table[NLMSG_HDRLEN] = NFPROTO_IPV4;
  AppendAttr(&flush_table, NFTA_RULE_TABLE, "dkc035_rule");
  mutate(flush_table, seq + 32, kNftDelrule);
  ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                      reinterpret_cast<sockaddr*>(&address), address_len));
  ready.revents = 0;
  EXPECT_EQ(0, poll(&ready, 1, 150));

  auto remove = TableBatch(kNftDeltable, seq + 35, "dkc035_rule");
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(nft.get(), remove));
  Ack(nft.get(), seq + 36, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, NftTableLookupDoesNotTruncateEmbeddedNul) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  auto create = TableBatch(kNftNewtable, 270, "dkc035_nul");
  ASSERT_EQ(static_cast<ssize_t>(create.size()), Send(fd.get(), create));
  Ack(fd.get(), 271, 0, kNftNewtable);

  auto query = Request(kNftGettable, 273);
  query[NLMSG_HDRLEN] = NFPROTO_IPV4;
  AppendAttr(&query, NFTA_TABLE_NAME, "dkc035_nulx");
  const size_t name_offset = NLMSG_HDRLEN + sizeof(nfgenmsg) + sizeof(nlattr);
  query[name_offset + std::strlen("dkc035_nul")] = 0;
  query[name_offset + std::strlen("dkc035_nulx")] = 'x';
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(fd.get(), query));
  Ack(fd.get(), 273, ENOENT, kNftGettable);

  auto remove = TableBatch(kNftDeltable, 275, "dkc035_nul");
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(fd.get(), remove));
  Ack(fd.get(), 276, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, NftTableCommitNotifiesSubscribersAndGeneration) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd listener(Open());
  Fd writer(Open());
  ASSERT_GE(listener.get(), 0);
  ASSERT_GE(writer.get(), 0);
  ASSERT_EQ(0, Bind(listener.get()));
  ASSERT_EQ(0, Bind(writer.get()));
  int group = NFNLGRP_NFTABLES;
  ASSERT_EQ(0, setsockopt(listener.get(), SOL_NETLINK, NETLINK_ADD_MEMBERSHIP,
                           &group, sizeof(group)));

  auto receive_events = [&](uint16_t event) {
    bool saw_table = false;
    bool saw_generation = false;
    for (int attempt = 0; attempt < 4 && (!saw_table || !saw_generation); ++attempt) {
      pollfd p{listener.get(), POLLIN, 0};
      ASSERT_EQ(1, poll(&p, 1, 2000));
      std::array<uint8_t, 4096> bytes{};
      const ssize_t count = recv(listener.get(), bytes.data(), bytes.size(), 0);
      ASSERT_GT(count, 0);
      int remaining = count;
      for (nlmsghdr* reply = reinterpret_cast<nlmsghdr*>(bytes.data());
           NLMSG_OK(reply, remaining); reply = NLMSG_NEXT(reply, remaining)) {
        if (reply->nlmsg_type == event) saw_table = true;
        if (reply->nlmsg_type == kNftNewgen) saw_generation = true;
      }
    }
    EXPECT_TRUE(saw_table);
    EXPECT_TRUE(saw_generation);
  };

  auto create = TableBatch(kNftNewtable, 280, "dkc035_notify");
  ASSERT_EQ(static_cast<ssize_t>(create.size()), Send(writer.get(), create));
  Ack(writer.get(), 281, 0, kNftNewtable);
  receive_events(kNftNewtable);

  auto remove = TableBatch(kNftDeltable, 283, "dkc035_notify");
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(writer.get(), remove));
  Ack(writer.get(), 284, 0, kNftDeltable);
  receive_events(kNftDeltable);
}

// A DELSETELEM multicast message must describe the element that was removed,
// including its old map value; it cannot be reconstructed from the new set.
TEST(NetlinkNetfilter, NftDeletedMapElementNotificationKeepsOldValue) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd writer(Open());
  Fd listener(Open());
  ASSERT_GE(writer.get(), 0);
  ASSERT_GE(listener.get(), 0);
  ASSERT_EQ(0, Bind(writer.get()));
  ASSERT_EQ(0, Bind(listener.get()));

  constexpr uint32_t seq = 19100;
  constexpr char table[] = "dkc035_del_notify";
  constexpr char map[] = "old_values";
  in_addr key{}, value{};
  ASSERT_EQ(1, inet_pton(AF_INET, "127.0.0.1", &key));
  ASSERT_EQ(1, inet_pton(AF_INET, "127.0.0.2", &value));
  auto create = TableBatch(kNftNewtable, seq, table);
  create.resize(create.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto new_map = Ipv4SourceSet(seq + 2, table, map, NFT_SET_MAP);
  auto new_element = Ipv4SourceSetElement(kNftNewsetelem, seq + 3, table, map,
                                          key, &value);
  auto end = Request(NFNL_MSG_BATCH_END, seq + 4);
  create.insert(create.end(), new_map.begin(), new_map.end());
  create.insert(create.end(), new_element.begin(), new_element.end());
  create.insert(create.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(create.size()), Send(writer.get(), create));
  Ack(writer.get(), seq + 1, 0, kNftNewtable);
  Ack(writer.get(), seq + 2, 0, kNftNewset);
  Ack(writer.get(), seq + 3, 0, kNftNewsetelem);

  int group = NFNLGRP_NFTABLES;
  ASSERT_EQ(0, setsockopt(listener.get(), SOL_NETLINK, NETLINK_ADD_MEMBERSHIP,
                           &group, sizeof(group)));
  auto remove = Request(NFNL_MSG_BATCH_BEGIN, seq + 5);
  nfgenmsg gen{};
  gen.res_id = htons(NFNL_SUBSYS_NFTABLES);
  std::memcpy(remove.data() + NLMSG_HDRLEN, &gen, sizeof(gen));
  auto old_element = Ipv4SourceSetElement(kNftDelsetelem, seq + 6, table, map, key);
  end = Request(NFNL_MSG_BATCH_END, seq + 7);
  remove.insert(remove.end(), old_element.begin(), old_element.end());
  remove.insert(remove.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(writer.get(), remove));
  Ack(writer.get(), seq + 6, 0, kNftDelsetelem);

  bool saw_delete = false;
  bool kept_value = false;
  for (int attempt = 0; attempt < 4 && !saw_delete; ++attempt) {
    pollfd ready{listener.get(), POLLIN, 0};
    ASSERT_EQ(1, poll(&ready, 1, 2000));
    std::array<uint8_t, 4096> bytes{};
    const ssize_t count = recv(listener.get(), bytes.data(), bytes.size(), 0);
    ASSERT_GT(count, 0);
    int remaining = count;
    for (auto* message = reinterpret_cast<nlmsghdr*>(bytes.data());
         NLMSG_OK(message, remaining); message = NLMSG_NEXT(message, remaining)) {
      if (message->nlmsg_type != kNftDelsetelem) continue;
      saw_delete = true;
      const auto attrs = FindNla(
          {reinterpret_cast<const uint8_t*>(NLMSG_DATA(message)) + sizeof(nfgenmsg),
           static_cast<size_t>(NLMSG_PAYLOAD(message, sizeof(nfgenmsg)))},
          NFTA_SET_ELEM_LIST_ELEMENTS);
      const auto element = FindNla(attrs, NFTA_LIST_ELEM);
      const auto data = FindNla(element, NFTA_SET_ELEM_DATA);
      const auto old_value = FindNla(data, NFTA_DATA_VALUE);
      kept_value = old_value.length == sizeof(value.s_addr) &&
                   std::memcmp(old_value.data, &value.s_addr, sizeof(value.s_addr)) == 0;
    }
  }
  EXPECT_TRUE(saw_delete);
  EXPECT_TRUE(kept_value);

  auto remove_table = TableBatch(kNftDeltable, seq + 8, table);
  ASSERT_EQ(static_cast<ssize_t>(remove_table.size()), Send(writer.get(), remove_table));
  Ack(writer.get(), seq + 9, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, NftDeletingTableNotifiesExistingRulesAndChains) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd writer(Open());
  Fd listener(Open());
  ASSERT_GE(writer.get(), 0);
  ASSERT_GE(listener.get(), 0);
  ASSERT_EQ(0, Bind(writer.get()));
  ASSERT_EQ(0, Bind(listener.get()));

  constexpr uint32_t seq = 294;
  auto create = TableBatch(kNftNewtable, seq, "dkc035_delete_tree");
  create.resize(create.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto chain = NewChain(seq + 2, "dkc035_delete_tree", "regular");
  auto rule = ImmediateRule(seq + 3, "dkc035_delete_tree", "regular",
                            NF_ACCEPT, true);
  create.insert(create.end(), chain.begin(), chain.end());
  create.insert(create.end(), rule.begin(), rule.end());
  auto end = Request(NFNL_MSG_BATCH_END, seq + 4);
  create.insert(create.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(create.size()), Send(writer.get(), create));
  Ack(writer.get(), seq + 1, 0, kNftNewtable);
  Ack(writer.get(), seq + 2, 0, kNftNewchain);
  Ack(writer.get(), seq + 3, 0, kNftNewrule);

  int group = NFNLGRP_NFTABLES;
  ASSERT_EQ(0, setsockopt(listener.get(), SOL_NETLINK, NETLINK_ADD_MEMBERSHIP,
                           &group, sizeof(group)));
  auto remove = TableBatch(kNftDeltable, seq + 5, "dkc035_delete_tree");
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(writer.get(), remove));
  Ack(writer.get(), seq + 6, 0, kNftDeltable);

  bool saw_rule = false, saw_chain = false, saw_table = false, saw_generation = false;
  for (int attempt = 0; attempt < 8 &&
                        !(saw_rule && saw_chain && saw_table && saw_generation);
       ++attempt) {
    pollfd ready{listener.get(), POLLIN, 0};
    ASSERT_EQ(1, poll(&ready, 1, 2000));
    std::array<uint8_t, 4096> bytes{};
    const ssize_t count = recv(listener.get(), bytes.data(), bytes.size(), 0);
    ASSERT_GT(count, 0);
    int remaining = count;
    for (auto* message = reinterpret_cast<nlmsghdr*>(bytes.data());
         NLMSG_OK(message, remaining); message = NLMSG_NEXT(message, remaining)) {
      saw_rule |= message->nlmsg_type == kNftDelrule;
      saw_chain |= message->nlmsg_type == kNftDelchain;
      saw_table |= message->nlmsg_type == kNftDeltable;
      saw_generation |= message->nlmsg_type == kNftNewgen;
    }
  }
  EXPECT_TRUE(saw_rule);
  EXPECT_TRUE(saw_chain);
  EXPECT_TRUE(saw_table);
  EXPECT_TRUE(saw_generation);
}

TEST(NetlinkNetfilter, NftSubscribedWriterReceivesOwnNotificationsOnce) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd writer(Open());
  ASSERT_GE(writer.get(), 0);
  ASSERT_EQ(0, Bind(writer.get()));
  int group = NFNLGRP_NFTABLES;
  ASSERT_EQ(0, setsockopt(writer.get(), SOL_NETLINK, NETLINK_ADD_MEMBERSHIP,
                           &group, sizeof(group)));

  auto expect_commit = [&](uint16_t event, uint32_t mutation_seq) {
    int objects = 0;
    int generations = 0;
    int acks = 0;
    for (int attempt = 0; attempt < 8 && (!objects || !generations || !acks); ++attempt) {
      pollfd p{writer.get(), POLLIN, 0};
      ASSERT_EQ(1, poll(&p, 1, 2000));
      std::array<uint8_t, 4096> bytes{};
      const ssize_t count = recv(writer.get(), bytes.data(), bytes.size(), 0);
      ASSERT_GT(count, 0);
      int remaining = count;
      for (nlmsghdr* reply = reinterpret_cast<nlmsghdr*>(bytes.data());
           NLMSG_OK(reply, remaining); reply = NLMSG_NEXT(reply, remaining)) {
        objects += reply->nlmsg_type == event;
        generations += reply->nlmsg_type == kNftNewgen;
        if (reply->nlmsg_type == NLMSG_ERROR && reply->nlmsg_seq == mutation_seq) {
          nlmsgerr ack{};
          ASSERT_GE(reply->nlmsg_len, NLMSG_LENGTH(sizeof(ack)));
          std::memcpy(&ack, NLMSG_DATA(reply), sizeof(ack));
          EXPECT_EQ(0, ack.error);
          ++acks;
        }
      }
    }
    EXPECT_EQ(1, objects);
    EXPECT_EQ(1, generations);
    EXPECT_EQ(1, acks);
    pollfd p{writer.get(), POLLIN, 0};
    EXPECT_EQ(0, poll(&p, 1, 0));
  };

  auto create = TableBatch(kNftNewtable, 310, "dkc035_self");
  ASSERT_EQ(static_cast<ssize_t>(create.size()), Send(writer.get(), create));
  expect_commit(kNftNewtable, 311);

  auto remove = TableBatch(kNftDeltable, 313, "dkc035_self");
  const size_t change_offset = NLMSG_SPACE(sizeof(nfgenmsg));
  for (size_t offset : {size_t{0}, change_offset}) {
    nlmsghdr header{};
    std::memcpy(&header, remove.data() + offset, sizeof(header));
    header.nlmsg_flags |= NLM_F_ECHO;
    std::memcpy(remove.data() + offset, &header, sizeof(header));
  }
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(writer.get(), remove));
  expect_commit(kNftDeltable, 314);
}

TEST(NetlinkNetfilter, NftFailedBatchDoesNotPublishEarlierTable) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  auto batch = TableBatch(kNftNewtable, 210, "dkc035_abort");
  batch.resize(batch.size() - NLMSG_SPACE(sizeof(nfgenmsg)));
  auto invalid = Request(kNftGetgen, 212);
  auto end = Request(NFNL_MSG_BATCH_END, 213);
  batch.insert(batch.end(), invalid.begin(), invalid.end());
  batch.insert(batch.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(fd.get(), batch));
  // Linux reports the earlier request's validation success even though the
  // later explicit error aborts the batch as a whole.
  Ack(fd.get(), 211, 0, kNftNewtable);
  Ack(fd.get(), 212, EINVAL, kNftGetgen);

  auto query = Request(kNftGettable, 214);
  query[NLMSG_HDRLEN] = NFPROTO_IPV4;
  AppendAttr(&query, NFTA_TABLE_NAME, "dkc035_abort");
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(fd.get(), query));
  Ack(fd.get(), 214, ENOENT, kNftGettable);
}

TEST(NetlinkNetfilter, NftShortBatchEndCannotCommitTable) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  auto batch = TableBatch(kNftNewtable, 220, "dkc035_shortend");
  const size_t end_offset = batch.size() - NLMSG_SPACE(sizeof(nfgenmsg));
  nlmsghdr end{};
  std::memcpy(&end, batch.data() + end_offset, sizeof(end));
  end.nlmsg_len = NLMSG_HDRLEN;
  std::memcpy(batch.data() + end_offset, &end, sizeof(end));
  ASSERT_EQ(static_cast<ssize_t>(batch.size()), Send(fd.get(), batch));
  pollfd p{fd.get(), POLLIN, 0};
  EXPECT_EQ(0, poll(&p, 1, 0));

  auto query = Request(kNftGettable, 223);
  query[NLMSG_HDRLEN] = NFPROTO_IPV4;
  AppendAttr(&query, NFTA_TABLE_NAME, "dkc035_shortend");
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(fd.get(), query));
  Ack(fd.get(), 223, ENOENT, kNftGettable);
}

TEST(NetlinkNetfilter, NftExistingTableReplaceIsRejected) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  auto create = TableBatch(kNftNewtable, 230, "dkc035_replace");
  ASSERT_EQ(static_cast<ssize_t>(create.size()), Send(fd.get(), create));
  Ack(fd.get(), 231, 0, kNftNewtable);

  auto replace = TableBatch(kNftNewtable, 233, "dkc035_replace");
  const size_t change_offset = NLMSG_SPACE(sizeof(nfgenmsg));
  nlmsghdr change{};
  std::memcpy(&change, replace.data() + change_offset, sizeof(change));
  change.nlmsg_flags &= ~NLM_F_EXCL;
  change.nlmsg_flags |= NLM_F_REPLACE;
  std::memcpy(replace.data() + change_offset, &change, sizeof(change));
  ASSERT_EQ(static_cast<ssize_t>(replace.size()), Send(fd.get(), replace));
  Ack(fd.get(), 234, EOPNOTSUPP, kNftNewtable);

  auto remove = TableBatch(kNftDeltable, 236, "dkc035_replace");
  ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(fd.get(), remove));
  Ack(fd.get(), 237, 0, kNftDeltable);
}

TEST(NetlinkNetfilter, NftTableDeleteByHandle) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  auto create = TableBatch(kNftNewtable, 240, "dkc035_handle");
  ASSERT_EQ(static_cast<ssize_t>(create.size()), Send(fd.get(), create));
  Ack(fd.get(), 241, 0, kNftNewtable);

  auto query = Request(kNftGettable, 243);
  query[NLMSG_HDRLEN] = NFPROTO_IPV4;
  AppendAttr(&query, NFTA_TABLE_NAME, "dkc035_handle");
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(fd.get(), query));
  std::array<uint8_t, 512> bytes{};
  const ssize_t n = recv(fd.get(), bytes.data(), bytes.size(), 0);
  ASSERT_GE(n, static_cast<ssize_t>(NLMSG_HDRLEN + sizeof(nfgenmsg)));
  std::array<uint8_t, 8> handle{};
  bool found = false;
  for (size_t offset = NLMSG_HDRLEN + sizeof(nfgenmsg);
       offset + sizeof(nlattr) <= static_cast<size_t>(n);) {
    nlattr attr{};
    std::memcpy(&attr, bytes.data() + offset, sizeof(attr));
    ASSERT_GE(attr.nla_len, sizeof(attr));
    ASSERT_LE(offset + attr.nla_len, static_cast<size_t>(n));
    if (attr.nla_type == NFTA_TABLE_HANDLE) {
      ASSERT_EQ(sizeof(attr) + handle.size(), attr.nla_len);
      std::memcpy(handle.data(), bytes.data() + offset + sizeof(attr), handle.size());
      found = true;
      break;
    }
    offset += NLA_ALIGN(attr.nla_len);
  }
  ASSERT_TRUE(found);
  Ack(fd.get(), 243, 0, kNftGettable);

  auto begin = Request(NFNL_MSG_BATCH_BEGIN, 245);
  nfgenmsg begin_gen{};
  begin_gen.res_id = htons(NFNL_SUBSYS_NFTABLES);
  std::memcpy(begin.data() + NLMSG_HDRLEN, &begin_gen, sizeof(begin_gen));
  auto change = Request(kNftDeltable, 246, sizeof(nfgenmsg) + NLA_ALIGN(12));
  change[NLMSG_HDRLEN] = NFPROTO_IPV4;
  nlattr attr{};
  attr.nla_type = NFTA_TABLE_HANDLE;
  attr.nla_len = 12;
  const size_t attr_offset = NLMSG_HDRLEN + sizeof(nfgenmsg);
  std::memcpy(change.data() + attr_offset, &attr, sizeof(attr));
  std::memcpy(change.data() + attr_offset + sizeof(attr), handle.data(), handle.size());
  auto end = Request(NFNL_MSG_BATCH_END, 247);
  begin.insert(begin.end(), change.begin(), change.end());
  begin.insert(begin.end(), end.begin(), end.end());
  ASSERT_EQ(static_cast<ssize_t>(begin.size()), Send(fd.get(), begin));
  Ack(fd.get(), 246, 0, kNftDeltable);
  ASSERT_EQ(static_cast<ssize_t>(query.size()), Send(fd.get(), query));
  Ack(fd.get(), 243, ENOENT, kNftGettable);
}

TEST(NetlinkNetfilter, NftTableDumpContinuesThroughMultipleObjects) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  for (const auto& [name, seq] : {
           std::pair<const char*, uint32_t>{"dkc035_dump_a", 250},
           {"dkc035_dump_b", 253},
       }) {
    auto create = TableBatch(kNftNewtable, seq, name);
    ASSERT_EQ(static_cast<ssize_t>(create.size()), Send(fd.get(), create));
    Ack(fd.get(), seq + 1, 0, kNftNewtable);
  }

  auto dump = Request(kNftGettable, 256);
  dump[NLMSG_HDRLEN] = NFPROTO_IPV4;
  nlmsghdr header{};
  std::memcpy(&header, dump.data(), sizeof(header));
  header.nlmsg_flags |= NLM_F_DUMP;
  std::memcpy(dump.data(), &header, sizeof(header));
  ASSERT_EQ(static_cast<ssize_t>(dump.size()), Send(fd.get(), dump));
  bool found_a = false;
  bool found_b = false;
  bool done = false;
  for (int page = 0; page < 8 && !done; ++page) {
    pollfd p{fd.get(), POLLIN, 0};
    ASSERT_EQ(1, poll(&p, 1, 2000));
    std::array<uint8_t, 4096> bytes{};
    const ssize_t count = recv(fd.get(), bytes.data(), bytes.size(), 0);
    ASSERT_GT(count, 0);
    int remaining = count;
    for (nlmsghdr* reply = reinterpret_cast<nlmsghdr*>(bytes.data());
         NLMSG_OK(reply, remaining); reply = NLMSG_NEXT(reply, remaining)) {
      EXPECT_EQ(256u, reply->nlmsg_seq);
      if (reply->nlmsg_type == NLMSG_DONE) {
        done = true;
        continue;
      }
      ASSERT_EQ(kNftNewtable, reply->nlmsg_type);
      EXPECT_NE(0, reply->nlmsg_flags & NLM_F_MULTI);
      const uint8_t* base = reinterpret_cast<uint8_t*>(NLMSG_DATA(reply));
      const size_t total = NLMSG_PAYLOAD(reply, sizeof(nfgenmsg));
      for (size_t offset = 0; offset + sizeof(nlattr) <= total;) {
        nlattr attr{};
        std::memcpy(&attr, base + sizeof(nfgenmsg) + offset, sizeof(attr));
        ASSERT_GE(attr.nla_len, sizeof(attr));
        ASSERT_LE(offset + attr.nla_len, total);
        if (attr.nla_type == NFTA_TABLE_NAME) {
          const char* name = reinterpret_cast<const char*>(
              base + sizeof(nfgenmsg) + offset + sizeof(attr));
          const size_t size = attr.nla_len - sizeof(attr);
          if (size == std::strlen("dkc035_dump_a") + 1 &&
              std::memcmp(name, "dkc035_dump_a", size) == 0) found_a = true;
          if (size == std::strlen("dkc035_dump_b") + 1 &&
              std::memcmp(name, "dkc035_dump_b", size) == 0) found_b = true;
        }
        offset += NLA_ALIGN(attr.nla_len);
      }
    }
  }
  EXPECT_TRUE(done);
  EXPECT_TRUE(found_a);
  EXPECT_TRUE(found_b);

  for (const auto& [name, seq] : {
           std::pair<const char*, uint32_t>{"dkc035_dump_a", 258},
           {"dkc035_dump_b", 261},
       }) {
    auto remove = TableBatch(kNftDeltable, seq, name);
    ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(fd.get(), remove));
    Ack(fd.get(), seq + 1, 0, kNftDeltable);
  }
}

TEST(NetlinkNetfilter, NftTableDumpAdvertisesMidstreamMutation) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd dump_fd(Open());
  Fd writer(Open());
  ASSERT_GE(dump_fd.get(), 0);
  ASSERT_GE(writer.get(), 0);
  ASSERT_EQ(0, Bind(dump_fd.get()));
  for (const auto& [name, seq] : {
           std::pair<const char*, uint32_t>{"dkc035_intr_a", 290},
           {"dkc035_intr_b", 293},
       }) {
    auto create = TableBatch(kNftNewtable, seq, name);
    ASSERT_EQ(static_cast<ssize_t>(create.size()), Send(writer.get(), create));
    Ack(writer.get(), seq + 1, 0, kNftNewtable);
  }

  auto dump = Request(kNftGettable, 296);
  dump[NLMSG_HDRLEN] = NFPROTO_IPV4;
  nlmsghdr header{};
  std::memcpy(&header, dump.data(), sizeof(header));
  header.nlmsg_flags |= NLM_F_DUMP;
  std::memcpy(dump.data(), &header, sizeof(header));
  ASSERT_EQ(static_cast<ssize_t>(dump.size()), Send(dump_fd.get(), dump));
  std::array<uint8_t, 4096> bytes{};
  ssize_t count = recv(dump_fd.get(), bytes.data(), bytes.size(), 0);
  ASSERT_GT(count, 0);
  bool done = false;
  size_t tables_in_first_datagram = 0;
  int remaining = count;
  for (nlmsghdr* reply = reinterpret_cast<nlmsghdr*>(bytes.data());
       NLMSG_OK(reply, remaining); reply = NLMSG_NEXT(reply, remaining)) {
    done |= reply->nlmsg_type == NLMSG_DONE;
    tables_in_first_datagram += reply->nlmsg_type == kNftNewtable;
  }

  // Linux can complete a tiny dump in the first datagram. DragonOS's bounded
  // continuation serves one page per recv, so this branch exercises DUMP_INTR.
  if (!done) {
    auto create = TableBatch(kNftNewtable, 298, "dkc035_intr_c");
    ASSERT_EQ(static_cast<ssize_t>(create.size()), Send(writer.get(), create));
    Ack(writer.get(), 299, 0, kNftNewtable);
    bool interrupted = false;
    for (int attempt = 0; attempt < 8 && !done; ++attempt) {
      pollfd p{dump_fd.get(), POLLIN, 0};
      ASSERT_EQ(1, poll(&p, 1, 2000));
      count = recv(dump_fd.get(), bytes.data(), bytes.size(), 0);
      ASSERT_GT(count, 0);
      remaining = count;
      for (nlmsghdr* reply = reinterpret_cast<nlmsghdr*>(bytes.data());
           NLMSG_OK(reply, remaining); reply = NLMSG_NEXT(reply, remaining)) {
        done |= reply->nlmsg_type == NLMSG_DONE;
        interrupted |= (reply->nlmsg_flags & NLM_F_DUMP_INTR) != 0;
      }
    }
    EXPECT_TRUE(done);
    if (tables_in_first_datagram < 2) {
      EXPECT_TRUE(interrupted);
    }
    auto remove = TableBatch(kNftDeltable, 301, "dkc035_intr_c");
    ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(writer.get(), remove));
    Ack(writer.get(), 302, 0, kNftDeltable);
  }

  for (const auto& [name, seq] : {
           std::pair<const char*, uint32_t>{"dkc035_intr_a", 304},
           {"dkc035_intr_b", 307},
       }) {
    auto remove = TableBatch(kNftDeltable, seq, name);
    ASSERT_EQ(static_cast<ssize_t>(remove.size()), Send(writer.get(), remove));
    Ack(writer.get(), seq + 1, 0, kNftDeltable);
  }
}

TEST(NetlinkNetfilter, ErrorAckAlignsPayloadAndPreservesOriginalHeader) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  ASSERT_EQ(0, Bind(fd.get()));
  const uint32_t actual_port = Port(fd.get());
  ASSERT_NE(0u, actual_port);
  const uint32_t forged_port = actual_port ^ 0xffffffffu;
  auto request = Request(kUnknown, 49, 5);
  nlmsghdr original{};
  std::memcpy(&original, request.data(), sizeof(original));
  original.nlmsg_pid = forged_port;
  std::memcpy(request.data(), &original, sizeof(original));
  request[NLMSG_HDRLEN + 4] = 0xa5;
  request.resize(original.nlmsg_len);
  ASSERT_EQ(21u, request.size());
  ASSERT_EQ(static_cast<ssize_t>(request.size()), Send(fd.get(), request));
  pollfd p{fd.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&p, 1, 2000));
  ASSERT_TRUE(p.revents & POLLIN);
  std::array<uint8_t, 128> bytes;
  bytes.fill(0xcc);
  sockaddr_nl sender{};
  socklen_t size = sizeof(sender);
  ASSERT_EQ(44, recvfrom(fd.get(), bytes.data(), bytes.size(), 0,
                         reinterpret_cast<sockaddr*>(&sender), &size));
  EXPECT_EQ(0u, sender.nl_pid);
  nlmsghdr reply{};
  nlmsgerr error{};
  std::memcpy(&reply, bytes.data(), sizeof(reply));
  std::memcpy(&error, bytes.data() + NLMSG_HDRLEN, sizeof(error));
  EXPECT_EQ(44u, reply.nlmsg_len);
  EXPECT_EQ(NLMSG_ERROR, reply.nlmsg_type);
  EXPECT_EQ(49u, reply.nlmsg_seq);
  EXPECT_EQ(actual_port, reply.nlmsg_pid);
  EXPECT_EQ(-EINVAL, error.error);
  EXPECT_EQ(21u, error.msg.nlmsg_len);
  EXPECT_EQ(forged_port, error.msg.nlmsg_pid);
  EXPECT_EQ(0, std::memcmp(bytes.data() + NLMSG_HDRLEN + sizeof(error.error),
                           request.data(), request.size()));
  for (size_t i = 41; i < 44; ++i) EXPECT_EQ(0, bytes[i]);
}

TEST(NetlinkNetfilter, BatchMissingSubsystemErrors) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  for (uint16_t subsystem : {uint16_t{255}, uint16_t{NFNL_SUBSYS_NONE}}) {
    auto request = Request(NFNL_MSG_BATCH_BEGIN, 50 + subsystem);
    nfgenmsg gen{};
    gen.res_id = htons(subsystem);
    std::memcpy(request.data() + NLMSG_HDRLEN, &gen, sizeof(gen));
    ASSERT_EQ(static_cast<ssize_t>(request.size()), Send(fd.get(), request));
    Ack(fd.get(), 50 + subsystem, subsystem == 255 ? EINVAL : EOPNOTSUPP,
        NFNL_MSG_BATCH_BEGIN);
  }
}

TEST(NetlinkNetfilter, ShortPayloadAckAndMalformedHeaderSilence) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  auto request = Request(kUnknown, 60, 0);
  ASSERT_EQ(static_cast<ssize_t>(request.size()), Send(fd.get(), request));
  // Linux nfnetlink_rcv_msg returns zero before subsystem dispatch here.
  Ack(fd.get(), 60, 0, kUnknown);
  request.resize(NLMSG_HDRLEN - 1);
  ASSERT_EQ(static_cast<ssize_t>(request.size()), Send(fd.get(), request));
  pollfd p{fd.get(), POLLIN, 0};
  EXPECT_EQ(0, poll(&p, 1, 50));
}

TEST(NetlinkNetfilter, PeekTruncAndQueueConsumption) {
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  const auto request = Request(kUnknown, 70);
  ASSERT_EQ(static_cast<ssize_t>(request.size()), Send(fd.get(), request));
  pollfd p{fd.get(), POLLIN, 0};
  ASSERT_EQ(1, poll(&p, 1, 2000));
  char byte;
  const ssize_t full = recv(fd.get(), &byte, 1, MSG_PEEK | MSG_TRUNC);
  ASSERT_GE(full, static_cast<ssize_t>(NLMSG_LENGTH(sizeof(nlmsgerr))));
  EXPECT_EQ(1, poll(&p, 1, 0));
  EXPECT_EQ(full, recv(fd.get(), &byte, 1, MSG_TRUNC));
  EXPECT_EQ(0, poll(&p, 1, 0));
  EXPECT_EQ(-1, recv(fd.get(), &byte, 1, 0));
  EXPECT_EQ(EAGAIN, errno);
}

TEST(NetlinkNetfilter, SendSizeEmptyAndOutOfBandBoundaries) {
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  auto request = Request(kUnknown, 75);
  sockaddr_nl kernel{};
  kernel.nl_family = AF_NETLINK;
  EXPECT_EQ(-1, sendto(fd.get(), request.data(), request.size(), MSG_OOB,
                      reinterpret_cast<sockaddr*>(&kernel), sizeof(kernel)));
  EXPECT_EQ(EOPNOTSUPP, errno);
  char byte;
  EXPECT_EQ(-1, recv(fd.get(), &byte, 1, MSG_OOB));
  EXPECT_EQ(EOPNOTSUPP, errno);
  EXPECT_EQ(-1, sendto(fd.get(), request.data(), 0, 0,
                      reinterpret_cast<sockaddr*>(&kernel), sizeof(kernel)));
  EXPECT_EQ(ENODATA, errno);
  // Well above the default socket send budget on both reference and guest;
  // this tests rejection, not a Linux promise about a fixed budget value.
  request.resize(16 * 1024 * 1024);
  EXPECT_EQ(-1, Send(fd.get(), request));
  EXPECT_EQ(EMSGSIZE, errno);
  pollfd p{fd.get(), POLLIN, 0};
  EXPECT_EQ(0, poll(&p, 1, 0));
}

TEST(NetlinkNetfilter, QueueOverrunReportsAndClearsErrorThenRecovers) {
  for (bool clear_with_sockopt : {false, true}) {
    Fd fd(Open());
    ASSERT_GE(fd.get(), 0);
    const auto request = Request(kUnknown, 76);
    pollfd p{fd.get(), POLLIN, 0};
    bool overrun = false;
    // Discover saturation instead of assuming Linux skb accounting matches
    // DragonOS's bounded queue. No rules or multicast subscribers are touched.
    for (size_t sent = 0; sent < 65536; ++sent) {
      ASSERT_EQ(static_cast<ssize_t>(request.size()), Send(fd.get(), request));
      ASSERT_EQ(1, poll(&p, 1, 0));
      if (p.revents & POLLERR) {
        overrun = true;
        break;
      }
    }
    ASSERT_TRUE(overrun) << "receive queue did not saturate within test budget";
    std::array<uint8_t, 512> bytes{};
    if (clear_with_sockopt) {
      int error = 0;
      socklen_t size = sizeof(error);
      ASSERT_EQ(0, getsockopt(fd.get(), SOL_SOCKET, SO_ERROR, &error, &size));
      EXPECT_EQ(ENOBUFS, error);
      ASSERT_EQ(0, getsockopt(fd.get(), SOL_SOCKET, SO_ERROR, &error, &size));
      EXPECT_EQ(0, error);
    } else {
      EXPECT_EQ(-1, recv(fd.get(), bytes.data(), bytes.size(), 0));
      EXPECT_EQ(ENOBUFS, errno);
    }
    ASSERT_EQ(1, poll(&p, 1, 0));
    EXPECT_EQ(0, p.revents & POLLERR);
    EXPECT_NE(0, p.revents & POLLIN);
    size_t drained = 0;
    for (; drained < 65536; ++drained) {
      const ssize_t n = recv(fd.get(), bytes.data(), bytes.size(), 0);
      if (n < 0) {
        ASSERT_EQ(EAGAIN, errno);
        break;
      }
      ASSERT_GE(n, static_cast<ssize_t>(NLMSG_LENGTH(sizeof(nlmsgerr))));
    }
    ASSERT_GT(drained, 0u);
    ASSERT_LT(drained, 65536u);
    EXPECT_EQ(0, poll(&p, 1, 0));
    const auto recovery = Request(kUnknown, 77);
    ASSERT_EQ(static_cast<ssize_t>(recovery.size()), Send(fd.get(), recovery));
    Ack(fd.get(), 77, NetAdmin() ? EINVAL : EPERM, kUnknown);
  }
}

TEST(NetlinkNetfilter, CongestionPersistsUntilReceiveQueueDrained) {
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  const auto request = Request(kUnknown, 78);
  pollfd p{fd.get(), POLLIN, 0};
  bool overrun = false;
  for (size_t sent = 0; sent < 65536; ++sent) {
    ASSERT_EQ(static_cast<ssize_t>(request.size()), Send(fd.get(), request));
    ASSERT_EQ(1, poll(&p, 1, 0));
    if (p.revents & POLLERR) {
      overrun = true;
      break;
    }
  }
  ASSERT_TRUE(overrun);
  int error = 0;
  socklen_t size = sizeof(error);
  ASSERT_EQ(0, getsockopt(fd.get(), SOL_SOCKET, SO_ERROR, &error, &size));
  ASSERT_EQ(ENOBUFS, error);
  const auto discarded = Request(kUnknown, 79);
  ASSERT_EQ(static_cast<ssize_t>(discarded.size()), Send(fd.get(), discarded));
  ASSERT_EQ(1, poll(&p, 1, 0));
  EXPECT_EQ(0, p.revents & POLLERR);
  // Freeing just one slot must not clear the congestion state.
  Ack(fd.get(), 78, NetAdmin() ? EINVAL : EPERM, kUnknown);
  ASSERT_EQ(static_cast<ssize_t>(discarded.size()), Send(fd.get(), discarded));
  ASSERT_EQ(1, poll(&p, 1, 0));
  EXPECT_EQ(0, p.revents & POLLERR);
  std::array<uint8_t, 512> bytes{};
  size_t drained = 0;
  for (; drained < 65536; ++drained) {
    const ssize_t n = recv(fd.get(), bytes.data(), bytes.size(), 0);
    if (n < 0) {
      ASSERT_EQ(EAGAIN, errno);
      break;
    }
    ASSERT_GE(n, static_cast<ssize_t>(sizeof(nlmsghdr)));
    nlmsghdr header{};
    std::memcpy(&header, bytes.data(), sizeof(header));
    EXPECT_EQ(78u, header.nlmsg_seq);
  }
  ASSERT_GT(drained, 0u);
  ASSERT_LT(drained, 65536u);
  EXPECT_EQ(0, poll(&p, 1, 0));
  ASSERT_EQ(static_cast<ssize_t>(discarded.size()), Send(fd.get(), discarded));
  Ack(fd.get(), 79, NetAdmin() ? EINVAL : EPERM, kUnknown);
}

TEST(NetlinkNetfilter, MembershipCapacityAndUnprivilegedRejection) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  // Linux allocates at least 32 groups even when nfnetlink defines fewer.
  for (int option : {NETLINK_ADD_MEMBERSHIP, NETLINK_DROP_MEMBERSHIP}) {
    int group = 32;
    EXPECT_EQ(0, setsockopt(fd.get(), SOL_NETLINK, option, &group, sizeof(group)));
    group = 33;
    EXPECT_EQ(-1, setsockopt(fd.get(), SOL_NETLINK, option, &group, sizeof(group)));
    EXPECT_EQ(EINVAL, errno);
  }
  Child([]() {
    Fd candidate(Open());
    if (candidate.get() < 0) return 1;
    // Reserve a currently free port, then release it before the failed bind.
    uint32_t port;
    {
      Fd reserve(Open());
      if (reserve.get() < 0 || Bind(reserve.get()) < 0) return 2;
      port = Port(reserve.get());
      if (!port) return 3;
    }
    __user_cap_header_struct header{};
    header.version = _LINUX_CAPABILITY_VERSION_3;
    __user_cap_data_struct data[2]{};
    if (syscall(SYS_capset, &header, data) < 0) return 4;
    for (int option : {NETLINK_ADD_MEMBERSHIP, NETLINK_DROP_MEMBERSHIP}) {
      int group = 32;
      if (setsockopt(candidate.get(), SOL_NETLINK, option, &group, sizeof(group)) != -1 ||
          errno != EPERM) return 5;
    }
    sockaddr_nl address{};
    address.nl_family = AF_NETLINK;
    address.nl_pid = port;
    address.nl_groups = uint32_t{1} << 31;
    if (bind(candidate.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)) != -1 ||
        errno != EPERM) return 6;
    if (Port(candidate.get()) != 0) return 7;
    Fd replacement(Open());
    if (replacement.get() < 0 || Bind(replacement.get(), port) < 0) return 8;
    return 0;
  });
}

TEST(NetlinkNetfilter, IgnoredMessagesHaveCappedSuccessfulAcknowledgements) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  for (bool control : {false, true}) {
    auto request = Request(control ? NLMSG_NOOP : kUnknown, 81);
    nlmsghdr header{};
    std::memcpy(&header, request.data(), sizeof(header));
    if (!control) header.nlmsg_flags = NLM_F_ACK;
    std::memcpy(request.data(), &header, sizeof(header));
    ASSERT_EQ(static_cast<ssize_t>(request.size()), Send(fd.get(), request));
    pollfd p{fd.get(), POLLIN, 0};
    ASSERT_EQ(1, poll(&p, 1, 2000));
    std::array<uint8_t, 256> bytes{};
    ASSERT_EQ(static_cast<ssize_t>(NLMSG_LENGTH(sizeof(nlmsgerr))),
              recv(fd.get(), bytes.data(), bytes.size(), 0));
    nlmsghdr reply{};
    nlmsgerr error{};
    std::memcpy(&reply, bytes.data(), sizeof(reply));
    std::memcpy(&error, bytes.data() + NLMSG_HDRLEN, sizeof(error));
    EXPECT_EQ(NLMSG_ERROR, reply.nlmsg_type);
    EXPECT_EQ(81u, reply.nlmsg_seq);
    EXPECT_NE(0, reply.nlmsg_flags & NLM_F_CAPPED);
    EXPECT_EQ(0, error.error);
    EXPECT_EQ(header.nlmsg_type, error.msg.nlmsg_type);
  }
}

TEST(NetlinkNetfilter, BatchShortGenerationIdIsRejectedBeforeDispatch) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  auto request = Request(NFNL_MSG_BATCH_BEGIN, 82,
                         sizeof(nfgenmsg) + NLA_ALIGN(NLA_HDRLEN + 1));
  nfgenmsg gen{};
  gen.res_id = htons(255);
  std::memcpy(request.data() + NLMSG_HDRLEN, &gen, sizeof(gen));
  nlattr attr{};
  attr.nla_type = NFNL_BATCH_GENID;
  attr.nla_len = NLA_HDRLEN + 1;
  std::memcpy(request.data() + NLMSG_HDRLEN + sizeof(gen), &attr, sizeof(attr));
  ASSERT_EQ(static_cast<ssize_t>(request.size()), Send(fd.get(), request));
  Ack(fd.get(), 82, ERANGE, NFNL_MSG_BATCH_BEGIN);
}

TEST(NetlinkNetfilter, DroppedSenderCapabilitiesRejectedOnExistingSocket) {
  Fd fd(Open());
  ASSERT_GE(fd.get(), 0);
  Child([&fd]() {
    __user_cap_header_struct header{};
    header.version = _LINUX_CAPABILITY_VERSION_3;
    __user_cap_data_struct data[2]{};
    if (syscall(SYS_capset, &header, data) < 0) return 1;
    const auto request = Request(kUnknown, 80);
    if (Send(fd.get(), request) != static_cast<ssize_t>(request.size())) return 2;
    pollfd p{fd.get(), POLLIN, 0};
    if (poll(&p, 1, 2000) != 1) return 3;
    std::array<uint8_t, 256> bytes{};
    if (recv(fd.get(), bytes.data(), bytes.size(), 0) <
        static_cast<ssize_t>(NLMSG_LENGTH(sizeof(nlmsgerr)))) return 4;
    nlmsgerr error{};
    std::memcpy(&error, bytes.data() + NLMSG_HDRLEN, sizeof(error));
    return error.error == -EPERM ? 0 : 5;
  });
}

TEST(NetlinkNetfilter, OpenerCredentialsAndExplicitDestinationException) {
  ASSERT_TRUE(NetAdmin()) << "requires CAP_NET_ADMIN";
  Child([]() {
    __user_cap_header_struct header{};
    header.version = _LINUX_CAPABILITY_VERSION_3;
    __user_cap_data_struct original[2]{};
    if (syscall(SYS_capget, &header, original) < 0) return 1;
    __user_cap_data_struct reduced[2]{};
    std::memcpy(reduced, original, sizeof(reduced));
    reduced[0].effective &= ~(uint32_t{1} << CAP_NET_ADMIN);
    if (syscall(SYS_capset, &header, reduced) < 0) return 2;
    Fd fd(Open());
    if (fd.get() < 0) return 3;
    if (syscall(SYS_capset, &header, original) < 0) return 4;
    const auto request = Request(kUnknown, 90);
    // An implicit destination checks both opener and sender credentials.
    // sendto's explicit kernel address sets NETLINK_SKB_DST in Linux,
    // bypassing only the opener check, never the sender check.
    for (bool explicit_destination : {false, true}) {
      const ssize_t sent = explicit_destination
          ? Send(fd.get(), request)
          : send(fd.get(), request.data(), request.size(), 0);
      if (sent != static_cast<ssize_t>(request.size())) return 5;
      pollfd p{fd.get(), POLLIN, 0};
      if (poll(&p, 1, 2000) != 1) return 6;
      std::array<uint8_t, 256> bytes{};
      if (recv(fd.get(), bytes.data(), bytes.size(), 0) <
          static_cast<ssize_t>(NLMSG_LENGTH(sizeof(nlmsgerr)))) return 7;
      nlmsgerr error{};
      std::memcpy(&error, bytes.data() + NLMSG_HDRLEN, sizeof(error));
      if (error.error != -(explicit_destination ? EINVAL : EPERM)) return 8;
    }
    return 0;
  });
}
}  // namespace

int main(int argc, char** argv) {
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
