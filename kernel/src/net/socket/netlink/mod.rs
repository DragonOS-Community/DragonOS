//https://code.dragonos.org.cn/xref/linux-6.1.9/net/netlink/
/*
..		-	-
Kconfig
Makefile
af_netlink.c
af_netlink.h
diag.c  Netlink 套接字的诊断功能，主要用于查询内核中存在的 Netlink 套接字信息
genetlink.c
policy.c
*/
// Top-level module defining the public API for Netlink
pub mod af_netlink;
pub mod skbuff;
pub mod netlink_proto;
