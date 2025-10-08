# PING
为DragonOS实现ping
## NAME
ping - 向网络主机发送ICMP ECHO_REQUEST
## SYNOPSIS
[-c count]： 指定 ping 的次数。例如，`-c 4` 会向目标主机发送 4 个 ping 请求。

[-i interval]：指定两次 ping 请求之间的时间间隔，单位是秒。例如，`-i 2` 会每 2 秒发送一次 ping 请求。

[-w timeout]： 指定等待 ping 响应的超时时间，单位是秒。例如，`-w 5` 会在 5 秒后超时。

[-s packetsize]：指定发送的 ICMP Packet 的大小，单位是字节。例如，`-s 64` 会发送大小为 64 字节的 ICMP Packet。

[-t ttl]：指定 ping 的 TTL (Time to Live)。例如，`-t 64` 会设置 TTL 为 64。

{destination}：指定要 ping 的目标主机。可以是 IP 地址或者主机名。例如，`192.168.1.1` 或 `www.example.com`。

## DESCRIPTION
ping 使用 ICMP 协议的必需的 ECHO_REQUEST 数据报来引发主机或网关的 ICMP ECHO_RESPONSE。ECHO_REQUEST 数据报（“ping”）具有 IP 和 ICMP 头，后面跟着一个 struct timeval，然后是用于填充数据包的任意数量的“填充”字节。

ping 支持 IPv4 和 IPv6。可以通过指定 -4 或 -6 来强制只使用其中一个。

ping 还可以发送 IPv6 节点信息查询（RFC4620）。可能不允许中间跳跃，因为 IPv6 源路由已被弃用（RFC5095）。
