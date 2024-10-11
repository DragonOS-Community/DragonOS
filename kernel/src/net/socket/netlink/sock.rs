#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SockFlags {
    Dead,
    Done,
    Urginline,
    Keepopen,
    Linger,
    Destroy,
    Broadcast,
    Timestamp,
    Zapped,
    UseWriteQueue,          // whether to call sk->sk_write_space in _wfree
    Dbg,                    // %SO_DEBUG setting
    Rcvtstamp,              // %SO_TIMESTAMP setting
    Rcvtstampns,            // %SO_TIMESTAMPNS setting
    Localroute,             // route locally only, %SO_DONTROUTE setting
    Memalloc,               // VM depends on this et for swapping
    TimestampingRxSoftware, // %SOF_TIMESTAMPING_RX_SOFTWARE
    Fasync,                 // fasync() active
    RxqOvfl,
    Zerocopy,   // buffers from userspace
    WifiStatus, // push wifi status to userspace
    Nofcs,      // Tell NIC not to do the Ethernet FCS.
    // Will use last 4 bytes of packet sent from
    // user-space instead.
    FilterLocked,   // Filter cannot be changed anymore
    SelectErrQueue, // Wake select on error queue
    RcuFree,        // wait rcu grace period in sk_destruct()
    Txtime,
    Xdp,       // XDP is attached
    TstampNew, // Indicates 64 bit timestamps always
    Rcvmark,   // Receive SO_MARK ancillary data with packet
}
