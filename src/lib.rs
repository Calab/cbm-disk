//! Read and write Commodore disk images (`.D64`, `.D71` and `.D81`) and
//! the files on them, as the 1541, 1571 and 1581 drives' CBM DOS keeps
//! them.
//!
//! # Where to start
//! * [`Volume`] is what most programs need: open a disk, then list,
//!   read, write, rename, delete and lock files, make directories (1581
//!   partitions), check the disk, or format a new one.
//!   [`Volume::features`] says what the disk can do.
//! * [`DiskImage`] is the disk itself, sector by sector (track and
//!   sector, as CBM DOS names them), with any error bytes the image
//!   carries.
//! * [`petscii`]: names and paths are PETSCII bytes, as the Commodore
//!   stores them; this converts to and from ASCII text.
//! * [`formats`]: how the images and the file system are laid out, byte
//!   by byte.
//!
//! The crate does no file I/O: read the image file's bytes, open them,
//! make changes, then save [`DiskImage::to_bytes`] the way your program
//! prefers. Every change either succeeds completely or leaves the disk as
//! it was. New files are given blocks the way the real drives choose
//! them, so a disk looks as if the drive wrote it.
//!
//! # Example
//! ```
//! use cbm_disk::{DiskImage, FileType, Layout, Volume};
//!
//! // A new 1581 disk (or: Volume::open(DiskImage::from_bytes(&bytes)?)?).
//! let mut vol = Volume::format(Layout::D81, b"MY DISK", *b"01")?;
//! vol.make_dir("GAMES", 3)?;
//! // A PRG file starts with its load address ($0801 for BASIC).
//! vol.write("GAMES/HELLO", FileType::Prg, &[0x01, 0x08, 0x00, 0x00])?;
//!
//! for entry in vol.list("GAMES")? {
//!     println!("{:<5} \"{}\" {}", entry.blocks, entry.name_text(), entry.type_text());
//! }
//! assert_eq!(vol.read("GAMES/HELLO")?, [0x01, 0x08, 0x00, 0x00]);
//! assert!(vol.validate().is_empty());
//!
//! let bytes = vol.image().to_bytes(); // ready to save as a .d81 file
//! # assert_eq!(bytes.len(), 819_200);
//! # Ok::<(), cbm_disk::Error>(())
//! ```
//!
//! # Errors
//! Every call that can fail returns [`Result`], with an [`Error`] whose
//! text (`to_string()`) is plain English, fit to show a user: "disk
//! full: 12 blocks needed, 3 free", "HELLO is locked".

#![warn(missing_docs)]

mod cbmdos;
mod check;
mod error;
pub mod formats;
mod image;
pub mod petscii;
mod volume;

pub use cbmdos::{ExtraTracks, FileType};
pub use check::{Problem, SectorUse};
pub use error::{Error, Result};
pub use image::{DiskImage, Layout, SECTOR_SIZE};
pub use volume::{DirEntry, Features, Geos, Volume};
