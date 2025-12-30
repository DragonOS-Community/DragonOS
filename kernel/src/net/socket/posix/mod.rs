// posix socket and arguments definitions
// now all posix definitions are with P front like MSG -> PMSG,
// for better understanding and avoiding conflicts with other definitions
mod ip_option;
mod ipv6_option;
mod msg_flag;
mod option;
mod option_level;
mod raw_option;
mod types;
mod uapi;

pub use ip_option::IpOption as PIP; // Socket options (SOL_IP)
pub use ipv6_option::Ipv6Option as PIPV6; // Socket options (SOL_IPV6)
pub use msg_flag::MessageFlag as PMSG; // Socket message flags MSG_*
pub use option::Options as PSO; // Socket options SO_*
pub use option_level::OptionLevel as PSOL; // Socket options level SOL_*
pub use raw_option::RawOption as PRAW; // Socket options (SOL_RAW)
pub use types::PSOCK; // Socket types SOCK_*
pub use uapi::IFNAMSIZ;
