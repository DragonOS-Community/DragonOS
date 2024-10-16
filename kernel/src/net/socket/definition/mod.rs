// socket definitions
mod option; 
mod option_level;
mod msg_flag;
mod types;

pub use option::Options; // Socket options SO_*
pub use option_level::OptionLevel; // Socket options level SOL_*
pub use msg_flag::MessageFlag; // Socket message flags MSG_*
pub use types::Type; // Socket types SOCK_*


