// Sock flags in Rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SockFlags {
    SockDead,
    SockDone,
    SockUrginline,
    SockKeepopen,
    SockLinger,
    SockDestroy,
    SockBroadcast,
    SockTimestamp,
    SockZapped,
    SockUseWriteQueue,          // whether to call sk->sk_write_space in sock_wfree
    SockDbg,                    // %SO_DEBUG setting
    SockRcvtstamp,              // %SO_TIMESTAMP setting
    SockRcvtstampns,            // %SO_TIMESTAMPNS setting
    SockLocalroute,             // route locally only, %SO_DONTROUTE setting
    SockMemalloc,               // VM depends on this socket for swapping
    SockTimestampingRxSoftware, // %SOF_TIMESTAMPING_RX_SOFTWARE
    SockFasync,                 // fasync() active
    SockRxqOvfl,
    SockZerocopy,   // buffers from userspace
    SockWifiStatus, // push wifi status to userspace
    SockNofcs,      // Tell NIC not to do the Ethernet FCS.
    // Will use last 4 bytes of packet sent from
    // user-space instead.
    SockFilterLocked,   // Filter cannot be changed anymore
    SockSelectErrQueue, // Wake select on error queue
    SockRcuFree,        // wait rcu grace period in sk_destruct()
    SockTxtime,
    SockXdp,       // XDP is attached
    SockTstampNew, // Indicates 64 bit timestamps always
    SockRcvmark,   // Receive SO_MARK ancillary data with packet
}
