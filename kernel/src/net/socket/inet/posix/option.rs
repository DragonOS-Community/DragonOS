
bitflags! {
    pub struct IpOptions: u32 {
        const IP_TOS = 1;                     // Type of service
        const IP_TTL = 2;                     // Time to live
        const IP_HDRINCL = 3;                 // Header compression
        const IP_OPTIONS = 4;                 // IP options
        const IP_ROUTER_ALERT = 5;            // Router alert
        const IP_RECVOPTS = 6;                // Receive options
        const IP_RETOPTS = 7;                 // Return options
        const IP_PKTINFO = 8;                 // Packet information
        const IP_PKTOPTIONS = 9;              // Packet options
        const IP_MTU_DISCOVER = 10;           // MTU discovery
        const IP_RECVERR = 11;                // Receive errors
        const IP_RECVTTL = 12;                // Receive time to live
        const IP_RECVTOS = 13;                // Receive type of service
        const IP_MTU = 14;                    // MTU
        const IP_FREEBIND = 15;               // Freebind
        const IP_IPSEC_POLICY = 16;           // IPsec policy
        const IP_XFRM_POLICY = 17;            // IPipsec transform policy
        const IP_PASSSEC = 18;                // Pass security
        const IP_TRANSPARENT = 19;            // Transparent

        const IP_RECVRETOPTS = 20;            // Receive return options (deprecated)

        const IP_ORIGDSTADDR = 21;            // Originate destination address (used by TProxy)
        const IP_RECVORIGDSTADDR = 21;        // Receive originate destination address

        const IP_MINTTL = 22;                 // Minimum time to live
        const IP_NODEFRAG = 23;               // Don't fragment (used by TProxy)
        const IP_CHECKSUM = 24;               // Checksum offload (used by TProxy)
        const IP_BIND_ADDRESS_NO_PORT = 25;   // Bind to address without port (used by TProxy)
        const IP_RECVFRAGSIZE = 26;           // Receive fragment size
        const IP_RECVERR_RFC4884 = 27;        // Receive ICMPv6 error notifications

        const IP_PMTUDISC_DONT = 28;          // Don't send DF frames
        const IP_PMTUDISC_DO = 29;            // Always DF
        const IP_PMTUDISC_PROBE = 30;         // Ignore dst pmtu
        const IP_PMTUDISC_INTERFACE = 31;     // Always use interface mtu (ignores dst pmtu)
        const IP_PMTUDISC_OMIT = 32;          // Weaker version of IP_PMTUDISC_INTERFACE

        const IP_MULTICAST_IF = 33;           // Multicast interface
        const IP_MULTICAST_TTL = 34;          // Multicast time to live
        const IP_MULTICAST_LOOP = 35;         // Multicast loopback
        const IP_ADD_MEMBERSHIP = 36;         // Add multicast group membership
        const IP_DROP_MEMBERSHIP = 37;        // Drop multicast group membership
        const IP_UNBLOCK_SOURCE = 38;         // Unblock source
        const IP_BLOCK_SOURCE = 39;           // Block source
        const IP_ADD_SOURCE_MEMBERSHIP = 40;  // Add source multicast group membership
        const IP_DROP_SOURCE_MEMBERSHIP = 41; // Drop source multicast group membership
        const IP_MSFILTER = 42;               // Multicast source filter

        const MCAST_JOIN_GROUP = 43;          // Join a multicast group
        const MCAST_BLOCK_SOURCE = 44;        // Block a multicast source
        const MCAST_UNBLOCK_SOURCE = 45;      // Unblock a multicast source
        const MCAST_LEAVE_GROUP = 46;         // Leave a multicast group
        const MCAST_JOIN_SOURCE_GROUP = 47;   // Join a multicast source group
        const MCAST_LEAVE_SOURCE_GROUP = 48;  // Leave a multicast source group
        const MCAST_MSFILTER = 49;           // Multicast source filter

        const IP_MULTICAST_ALL = 50;          // Multicast all
        const IP_UNICAST_IF = 51;             // Unicast interface
        const IP_LOCAL_PORT_RANGE = 52;       // Local port range
        const IP_PROTOCOL = 53;               // Protocol

        // ... other flags ...
    }
}