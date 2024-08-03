

bitflags! {
    pub struct TcpOptions: u32 {
        const TCP_NODELAY = 1;
        const TCP_MAXSEG = 2;
        const TCP_CORK = 3;
        const TCP_KEEPIDLE = 4;
        const TCP_KEEPINTVL = 5;
        const TCP_KEEPCNT = 6;
        const TCP_SYNCNT = 7;
        const TCP_LINGER2 = 8;
        const TCP_DEFER_ACCEPT = 9;
        const TCP_WINDOW_CLAMP = 10;
        const TCP_INFO = 11;
        const TCP_QUICKACK = 12;
        const TCP_CONGESTION = 13;
        const TCP_MD5SIG = 14;
        const TCP_THIN_LINEAR_TIMEOUTS = 16;
        const TCP_THIN_DUPACK = 17;
        const TCP_USER_TIMEOUT = 18;
        const TCP_REPAIR = 19;
        const TCP_REPAIR_QUEUE = 20;
        const TCP_QUEUE_SEQ = 21;
        const TCP_REPAIR_OPTIONS = 22;
        const TCP_FASTOPEN = 23;
        const TCP_TIMESTAMP = 24;
        const TCP_NOTSENT_LOWAT = 25;
        const TCP_CC_INFO = 26;
        const TCP_SAVE_SYN = 27;
        const TCP_SAVED_SYN = 28;
        const TCP_REPAIR_WINDOW = 29;
        const TCP_FASTOPEN_CONNECT = 30;
        const TCP_ULP = 31;
        const TCP_MD5SIG_EXT = 32;
        const TCP_FASTOPEN_KEY = 33;
        const TCP_FASTOPEN_NO_COOKIE = 34;
        const TCP_ZEROCOPY_RECEIVE = 35;
        const TCP_INQ = 36;
        const TCP_CM_INQ = Self::TCP_INQ.bits();
        const TCP_TX_DELAY = 37;
        const TCP_AO_ADD_KEY = 38;
        const TCP_AO_DEL_KEY = 39;
        const TCP_AO_INFO = 40;
        const TCP_AO_GET_KEYS = 41;
        const TCP_AO_REPAIR = 42;
    }
}

// // You can then define values with exact meanings like this:
// const TCP_REPAIR_ON: TcpOptions = TcpOptions::from_bits_truncate(1);
// const TCP_REPAIR_OFF: TcpOptions = TcpOptions::from_bits_truncate(0);
// const TCP_REPAIR_OFF_NO_WP: TcpOptions = TcpOptions::from_bits_truncate(-1);