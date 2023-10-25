/// An enumeration of the possible values for the `AT_*` constants.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum AtType {
    /// End of vector.
    Null,
    /// Entry should be ignored.
    Ignore,
    /// File descriptor of program.
    ExecFd,
    /// Program headers for program.
    Phdr,
    /// Size of program header entry.
    PhEnt,
    /// Number of program headers.
    PhNum,
    /// System page size.
    PageSize,
    /// Base address of interpreter.
    Base,
    /// Flags.
    Flags,
    /// Entry point of program.
    Entry,
    /// Program is not ELF.
    NotElf,
    /// Real uid.
    Uid,
    /// Effective uid.
    EUid,
    /// Real gid.
    Gid,
    /// Effective gid.
    EGid,
    /// String identifying CPU for optimizations.
    Platform,
    /// Arch dependent hints at CPU capabilities.
    HwCap,
    /// Frequency at which times() increments.
    ClkTck,
    /// Secure mode boolean.
    Secure,
    /// String identifying real platform, may differ from AT_PLATFORM.
    BasePlatform,
    /// Address of 16 random bytes.
    Random,
    /// Extension of AT_HWCAP.
    HwCap2,
    /// Filename of program.
    ExecFn,
    /// Minimal stack size for signal delivery.
    MinSigStackSize,
}

impl TryFrom<u32> for AtType {
    type Error = &'static str;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(AtType::Null),
            1 => Ok(AtType::Ignore),
            2 => Ok(AtType::ExecFd),
            3 => Ok(AtType::Phdr),
            4 => Ok(AtType::PhEnt),
            5 => Ok(AtType::PhNum),
            6 => Ok(AtType::PageSize),
            7 => Ok(AtType::Base),
            8 => Ok(AtType::Flags),
            9 => Ok(AtType::Entry),
            10 => Ok(AtType::NotElf),
            11 => Ok(AtType::Uid),
            12 => Ok(AtType::EUid),
            13 => Ok(AtType::Gid),
            14 => Ok(AtType::EGid),
            15 => Ok(AtType::Platform),
            16 => Ok(AtType::HwCap),
            17 => Ok(AtType::ClkTck),
            23 => Ok(AtType::Secure),
            24 => Ok(AtType::BasePlatform),
            25 => Ok(AtType::Random),
            26 => Ok(AtType::HwCap2),
            31 => Ok(AtType::ExecFn),
            51 => Ok(AtType::MinSigStackSize),
            _ => Err("Invalid value for AtType"),
        }
    }
}

bitflags! {
    pub struct WaitOption: u32{
        const WNOHANG = 0x00000001;
        const WUNTRACED = 0x00000002;
        const WSTOPPED = 0x00000002;
        const WEXITED = 0x00000004;
        const WCONTINUED = 0x00000008;
        const WNOWAIT = 0x01000000;
        const WNOTHREAD = 0x20000000;
        const WALL = 0x40000000;
        const WCLONE = 0x80000000;
    }
}
