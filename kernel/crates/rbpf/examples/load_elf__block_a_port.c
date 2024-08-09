// SPDX-License-Identifier: (APACHE-2.0 OR MIT)
// Copyright 2016 6WIND S.A. <quentin.monnet@6wind.com>

// Block TCP packets on source or destination port 0x9999.

#include <linux/ip.h>
#include <linux/in.h>
#include <linux/tcp.h>
#include <linux/bpf.h>

#define ETH_ALEN 6
#define ETH_P_IP 0x0008 /* htons(0x0800) */
#define TCP_HDR_LEN 20

#define BLOCKED_TCP_PORT 0x9999

struct eth_hdr {
    unsigned char   h_dest[ETH_ALEN];
    unsigned char   h_source[ETH_ALEN];
    unsigned short  h_proto;
};

#define SEC(NAME) __attribute__((section(NAME), used))
SEC(".classifier")
int handle_ingress(struct __sk_buff *skb)
{
    void *data = (void *)(long)skb->data;
    void *data_end = (void *)(long)skb->data_end;
    struct eth_hdr *eth = data;
    struct iphdr *iph = data + sizeof(*eth);
    struct tcphdr *tcp = data + sizeof(*eth) + sizeof(*iph);

    /* single length check */
    if (data + sizeof(*eth) + sizeof(*iph) + sizeof(*tcp) > data_end)
        return 0;
    if (eth->h_proto != ETH_P_IP)
        return 0;
    if (iph->protocol != IPPROTO_TCP)
        return 0;
    if (tcp->source == BLOCKED_TCP_PORT || tcp->dest == BLOCKED_TCP_PORT)
        return -1;
    return 0;
}
