// posix socket and arguments definitions
// now all posix definitions are with P front like MSG -> PMSG,
// for better understanding and avoiding conflicts with other definitions
mod msg_flag;
mod option;
mod option_level;
mod types;

pub use msg_flag::MessageFlag as PMSG; // Socket message flags MSG_*
pub use option::Options as PSO; // Socket options SO_*
pub use option_level::OptionLevel as PSOL; // Socket options level SOL_*
pub use types::Type as PSOCK; // Socket types SOCK_*
