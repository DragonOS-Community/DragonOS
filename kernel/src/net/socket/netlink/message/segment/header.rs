#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CMsgSegHdr {
    /// Length of the message, including the header
    pub len: u32,
    /// Type of message content
    pub type_: u16,
    /// Additional flags
    pub flags: u16,
    /// Sequence number
    pub seq: u32,
    /// Sending process port ID
    pub pid: u32,
}

bitflags! {
    pub struct SegHdrCommonFlags: u16 {
        /// Indicates a request message
        const REQUEST = 0x01;
        /// Multipart message, terminated by NLMSG_DONE
        const MULTI = 0x02;
        /// Reply with an acknowledgment, with zero or an error code
        const ACK = 0x04;
        /// Echo this request
        const ECHO = 0x08;
        /// Dump was inconsistent due to sequence change
        const DUMP_INTR = 0x10;
        /// Dump was filtered as requested
        const DUMP_FILTERED = 0x20;
    }
}

bitflags! {
    pub struct GetRequestFlags: u16 {
        /// Specify the tree root
        const ROOT = 0x100;
        /// Return all matching results
        const MATCH = 0x200;
        /// Atomic get request
        const ATOMIC = 0x400;
        /// Combination flag for root and match
        const DUMP = Self::ROOT.bits | Self::MATCH.bits;
    }
}

bitflags! {
    pub struct NewRequestFlags: u16 {
        /// Override existing entries
        const REPLACE = 0x100;
        /// Do not modify if it exists
        const EXCL = 0x200;
        /// Create if it does not exist
        const CREATE = 0x400;
        /// Add to the end of the list
        const APPEND = 0x800;
    }
}

bitflags! {
    pub struct DeleteRequestFlags: u16 {
        /// Do not delete recursively
        const NONREC = 0x100;
        /// Delete multiple objects
        const BULK = 0x200;
    }
}

bitflags! {
    pub struct AckFlags: u16 {
        const CAPPED = 0x100;
        const ACK_TLVS = 0x100;
    }
}
