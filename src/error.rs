//! The one error type the crate returns.

use std::fmt;

/// Everything that can go wrong reading or changing a disk image. More
/// kinds may be added, so a `match` needs a `_` arm.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The bytes aren't a disk image this crate understands; the text says
    /// why.
    NotAnImage(String),
    /// A valid image or request, but of a kind not supported (yet), e.g.
    /// writing a REL file.
    Unsupported(String),
    /// No such track and sector on this disk.
    SectorOutOfRange {
        /// The track asked for.
        track: u8,
        /// The sector asked for.
        sector: u8,
    },
    /// Data written to a sector must be exactly 256 bytes.
    WrongSectorLength {
        /// The length given.
        got: usize,
    },
    /// The disk's own records don't add up (e.g. a file's chain of blocks
    /// leads off the disk); the text says where.
    Damaged(String),
    /// No file or directory of that name.
    NotFound(String),
    /// A directory was given where a file is needed (e.g. to read it).
    IsDirectory(String),
    /// A file was given where a directory is needed (e.g. in a path).
    NotADirectory(String),
    /// A file or directory of that name is already there.
    AlreadyExists(String),
    /// The name breaks the disk's rules; the text says how.
    BadName(String),
    /// The file is locked, so it can't be written, renamed or deleted
    /// until it's unlocked.
    Locked(String),
    /// Not enough free blocks.
    DiskFull {
        /// Blocks the change needs.
        needed: u32,
        /// Blocks free.
        free: u32,
    },
    /// The directory has no room for another entry (144 on a 1541 or
    /// 1571 disk, 296 on a 1581).
    DirectoryFull,
    /// Only empty directories can be deleted.
    DirectoryNotEmpty(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotAnImage(why) => write!(f, "not a disk image: {why}"),
            Error::Unsupported(what) => write!(f, "not supported: {what}"),
            Error::SectorOutOfRange { track, sector } => {
                write!(f, "track {track} sector {sector} is not on this disk")
            }
            Error::WrongSectorLength { got } => {
                write!(f, "a sector holds 256 bytes, but {got} were given")
            }
            Error::Damaged(why) => write!(f, "damaged: {why}"),
            Error::NotFound(name) => write!(f, "{name} not found"),
            Error::IsDirectory(name) => write!(f, "{name} is a directory"),
            Error::NotADirectory(name) => write!(f, "{name} is not a directory"),
            Error::AlreadyExists(name) => write!(f, "{name} already exists"),
            Error::BadName(why) => write!(f, "bad name: {why}"),
            Error::Locked(name) => write!(f, "{name} is locked"),
            Error::DiskFull { needed, free } => {
                write!(f, "disk full: {needed} blocks needed, {free} free")
            }
            Error::DirectoryFull => write!(f, "the directory is full"),
            Error::DirectoryNotEmpty(name) => write!(f, "{name} is not empty"),
        }
    }
}

impl std::error::Error for Error {}

/// Shorthand for results from this crate.
pub type Result<T> = std::result::Result<T, Error>;
