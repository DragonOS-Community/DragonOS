bitflags::bitflags! {
    /// # Message Flags
    /// Flags we can use with send/ and recv. \
    /// Added those for 1003.1g not all are supported yet
    /// ## Reference
    /// - [Linux Socket Flags](https://code.dragonos.org.cn/xref/linux-6.6.21/include/linux/socket.h#299)
    pub struct MessageFlag: u32 {
        /// `MSG_OOB`
        /// `0b0000_0001`\
        /// Process out-of-band data.
        const OOB       = 1;
        /// `MSG_PEEK`
        /// `0b0000_0010`\
        /// Peek at an incoming message.
        const PEEK      = 2;
        /// `MSG_DONTROUTE`
        /// `0b0000_0100`\
        /// Don't use routing tables.
        const DONTROUTE = 4;
        /// `MSG_TRYHARD`
        /// `0b0000_0100`\
        /// `MSG_TRYHARD` is not defined in the standard, but it is used in Linux.
        const TRYHARD   = 4;
        /// `MSG_CTRUNC`
        /// `0b0000_1000`\
        /// Control data lost before delivery.
        const CTRUNC     = 8;
        /// `MSG_PROBE`
        /// `0b0001_0000`\
        const PROBE     = 0x10;
        /// `MSG_TRUNC`
        /// `0b0010_0000`\
        /// Data truncated before delivery.
        const TRUNC     = 0x20;
        /// `MSG_DONTWAIT`
        /// `0b0100_0000`\
        /// This flag is used to make the socket non-blocking.
        const DONTWAIT  = 0x40;
        /// `MSG_EOR`
        /// `0b1000_0000`\
        /// End of record.
        const EOR       = 0x80;
        /// `MSG_WAITALL`
        /// `0b0001_0000_0000`\
        /// Wait for full request or error.
        const WAITALL   = 0x100;
        /// `MSG_FIN`
        /// `0b0010_0000_0000`\
        /// Terminate the connection.
        const FIN       = 0x200;
        /// `MSG_SYN`
        /// `0b0100_0000_0000`\
        /// Synchronize sequence numbers.
        const SYN       = 0x400;
        /// `MSG_CONFIRM`
        /// `0b1000_0000_0000`\
        /// Confirm path validity.
        const CONFIRM   = 0x800;
        /// `MSG_RST`
        /// `0b0001_0000_0000_0000`\
        /// Reset the connection.
        const RST       = 0x1000;
        /// `MSG_ERRQUEUE`
        /// `0b0010_0000_0000_0000`\
        /// Fetch message from error queue.
        const ERRQUEUE  = 0x2000;
        /// `MSG_NOSIGNAL`
        /// `0b0100_0000_0000_0000`\
        /// Do not generate a signal.
        const NOSIGNAL  = 0x4000;
        /// `MSG_MORE`
        /// `0b1000_0000_0000_0000`\
        /// Sender will send more.
        const MORE      = 0x8000;
        /// `MSG_WAITFORONE`
        /// `0b0001_0000_0000_0000_0000`\
        /// For nonblocking operation.
        const WAITFORONE = 0x10000;
        /// `MSG_SENDPAGE_NOPOLICY`
        /// `0b0010_0000_0000_0000_0000`\
        /// Sendpage: do not apply policy.
        const SENDPAGE_NOPOLICY = 0x10000;
        /// `MSG_BATCH`
        /// `0b0100_0000_0000_0000_0000`\
        /// Sendpage: next message is batch.
        const BATCH     = 0x40000;
        /// `MSG_EOF`
        const EOF       = Self::FIN.bits;
        /// `MSG_NO_SHARED_FRAGS`
        const NO_SHARED_FRAGS = 0x80000;
        /// `MSG_SENDPAGE_DECRYPTED`
        const SENDPAGE_DECRYPTED = 0x10_0000;

        /// `MSG_ZEROCOPY`
        const ZEROCOPY      = 0x400_0000;
        /// `MSG_SPLICE_PAGES`
        const SPLICE_PAGES  = 0x800_0000;
        /// `MSG_FASTOPEN`
        const FASTOPEN      = 0x2000_0000;
        /// `MSG_CMSG_CLOEXEC`
        const CMSG_CLOEXEC  = 0x4000_0000;
        /// `MSG_CMSG_COMPAT`
        // if define CONFIG_COMPAT
        // const CMSG_COMPAT   = 0x8000_0000;
        const CMSG_COMPAT   = 0;
        /// `MSG_INTERNAL_SENDMSG_FLAGS`
        const INTERNAL_SENDMSG_FLAGS
            = Self::SPLICE_PAGES.bits | Self::SENDPAGE_NOPOLICY.bits | Self::SENDPAGE_DECRYPTED.bits;
    }
}
