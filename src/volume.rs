//! [`Volume`]: one disk and its CBM DOS file system, behind the calls a
//! program uses (list, read, write, rename, delete, lock, make a
//! directory, format, ...).

use std::collections::HashSet;
use std::ops::Range;

use crate::cbmdos::{self, Allocator, Bam, Dir, ExtraTracks, FileSystem, FileType, Raw};
use crate::check::{self, Problem, SectorUse};
use crate::error::{Error, Result};
use crate::image::{DiskImage, Layout, SECTOR_SIZE};
use crate::petscii::{SHIFTED_SPACE, petscii_to_ascii, petscii_to_ascii_lower};

/// What a disk can do, from [`Volume::features`], e.g. to show only the
/// buttons that work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Features {
    /// Files at all (list, read, write, rename, delete): false unless
    /// the disk holds [`FileSystem::CbmDos`].
    pub files: bool,
    /// Directories (1581 partitions): `/` paths, [`Volume::make_dir`],
    /// [`Volume::move_to`]. D81 only.
    pub directories: bool,
    /// Locked files: [`Volume::lock`], [`Volume::unlock`].
    pub locking: bool,
    /// The most entries the main directory holds: 144 on 1541 and 1571
    /// disks, 296 on a 1581.
    pub max_entries: u32,
    /// The file types [`Volume::write`] can make.
    pub file_types: &'static [FileType],
}

impl FileSystem {
    /// What a disk of `layout` holding this file system can do. Without
    /// a disk, e.g. for a "New disk" dialog:
    /// `FileSystem::CbmDos.features(Layout::D81).directories`.
    pub fn features(self, layout: Layout) -> Features {
        if self != FileSystem::CbmDos {
            return Features {
                files: false,
                directories: false,
                locking: false,
                max_entries: 0,
                file_types: &[],
            };
        }
        let d81 = layout == Layout::D81;
        Features {
            files: true,
            directories: d81,
            locking: true,
            max_entries: if d81 { 296 } else { 144 },
            file_types: &[FileType::Prg, FileType::Seq, FileType::Usr],
        }
    }
}

/// GEOS details of a file, from its directory entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Geos {
    /// GEOS's own file type (byte `$18`): e.g. 1 BASIC, 4 system, 6
    /// application, 7 application data, 8 font, 9 printer driver, 11
    /// desk accessory.
    pub file_type: u8,
    /// A VLIR file (in records) rather than a plain sequential one.
    /// Only the record index is read for now (see [`Volume::read`]).
    pub vlir: bool,
    /// Where its info block (icon, description, ...) is.
    pub info_block: (u8, u8),
    /// When it was written: year (e.g. 1991), month, day, hour, minute,
    /// as stored (not checked).
    pub date: (u16, u8, u8, u8, u8),
}

/// One entry in a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DirEntry {
    /// The name as stored (PETSCII, without the `$A0` padding): give it
    /// back in a path to reach the entry. See [`DirEntry::name_text`] to
    /// show it.
    pub name: Vec<u8>,
    /// PRG, SEQ, USR, REL, DEL or CBM.
    pub file_type: FileType,
    /// Closed properly. One that isn't is a "splat" file (`*PRG` in a
    /// listing), left by a program that never finished writing it.
    pub closed: bool,
    /// Locked (`<` in a listing): it can't be written, renamed or
    /// deleted until it's unlocked.
    pub locked: bool,
    /// The block count the directory shows. It isn't checked: directory
    /// art and copy protection often fake it.
    pub blocks: u16,
    /// The length in bytes: the data in its chain of blocks (a VLIR
    /// file's records added up; for a 1581 partition, 256 bytes a block).
    pub size: u32,
    /// Its chain of blocks can't be followed to the end, so reading it
    /// fails (a splat file is read as far as it goes).
    pub damaged: bool,
    /// Its first block (for a 1581 partition, its first sector).
    pub start: (u8, u8),
    /// REL files: the length of each record.
    pub record_length: Option<u8>,
    /// GEOS files: their GEOS details.
    pub geos: Option<Geos>,
    /// A 1581 partition laid out as a subdirectory, so it can be listed
    /// and written into.
    pub directory: bool,
    /// The 30 bytes of the directory entry as stored (from the type byte
    /// at `$02` to the block count at `$1E`), for tools that want more.
    pub raw: [u8; 30],
}

impl DirEntry {
    /// The name as ASCII text to show, as the C64's power-on character
    /// set draws it (see [`crate::petscii::petscii_to_ascii`]).
    pub fn name_text(&self) -> String {
        petscii_to_ascii(&self.name)
    }

    /// The name as ASCII text in the lower/upper case character set (see
    /// [`crate::petscii::petscii_to_ascii_lower`]).
    pub fn name_text_lower(&self) -> String {
        petscii_to_ascii_lower(&self.name)
    }

    /// A directory (a 1581 partition laid out as one), not a file.
    pub fn is_dir(&self) -> bool {
        self.directory
    }

    /// The type as a directory listing shows it: `"PRG"`, `"*SEQ"` when
    /// not closed, `"PRG<"` when locked.
    pub fn type_text(&self) -> String {
        format!(
            "{}{}{}",
            if self.closed { "" } else { "*" },
            self.file_type.letters(),
            if self.locked { "<" } else { "" }
        )
    }
}

/// A disk image and the CBM DOS file system on it. The volume owns the
/// image; [`Volume::image`] gives it back to save, e.g. with
/// [`DiskImage::to_bytes`].
///
/// Names and paths are PETSCII bytes (see [`crate::petscii`]). Ordinary
/// names in capitals can be given as Rust strings. On a 1581 disk, `/`
/// separates a partition (subdirectory) from what's in it, as in the
/// 1581's own commands: `"GAMES/PACMAN"`. `/` is also a legal character
/// in a name, so a `/` splits a path only where the part before it is a
/// directory; `"A/B"` names a file of that name when there's no
/// directory `A`.
///
/// Every change either succeeds completely or leaves the disk as it was.
/// Changes are refused with [`Error::Damaged`] if the directory they
/// touch can't be followed, and new files never take a block something
/// else uses, even if the BAM says it's free. Blocks are chosen as the
/// drive itself chooses them, so a disk looks as if a real drive wrote
/// it.
///
/// ```
/// use cbm_disk::{FileType, Layout, Volume};
///
/// let mut vol = Volume::format(Layout::D64, b"MY DISK", *b"01")?;
/// vol.write("HELLO", FileType::Seq, b"HELLO, COMMODORE\r")?;
/// assert_eq!(vol.read("HELLO")?, b"HELLO, COMMODORE\r");
/// assert_eq!(vol.blocks_free(), 663);
/// let bytes = vol.image().to_bytes(); // ready to save as a .d64 file
/// # Ok::<(), cbm_disk::Error>(())
/// ```
#[derive(Debug, Clone)]
pub struct Volume {
    image: DiskImage,
    fs: FileSystem,
    overwrite: bool,
}

impl Volume {
    /// Which file system, if any, `image` holds: [`FileSystem::CbmDos`],
    /// [`FileSystem::CpM`], or `None` when it's neither (e.g. an
    /// unformatted disk). [`Volume::open`] takes the image, so ask this
    /// first if you want to keep the image when it isn't one this crate
    /// can read.
    ///
    /// Only the header and the first directory sector are looked at, so
    /// a CBM disk that's damaged elsewhere still counts as CBM DOS (and
    /// [`Volume::validate`] reports the damage).
    pub fn detect(image: &DiskImage) -> Option<FileSystem> {
        cbmdos::detect(image)
    }

    /// Open the file system on `image`. When [`Volume::detect`] finds
    /// none, the volume is [`FileSystem::Raw`]. On a raw or CP/M disk the
    /// sector calls work and the file calls give [`Error::Unsupported`]
    /// ([`Volume::features`] says which).
    ///
    /// ```no_run
    /// use cbm_disk::{DiskImage, FileSystem, Volume};
    ///
    /// let bytes = std::fs::read("GAMES.D64")?;
    /// let vol = Volume::open(DiskImage::from_bytes(&bytes)?)?;
    /// match vol.file_system() {
    ///     FileSystem::CbmDos => println!("{} blocks free", vol.blocks_free()),
    ///     other => println!("a {} disk: sectors only", other.name()),
    /// }
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn open(image: DiskImage) -> Result<Self> {
        let fs = Self::detect(&image).unwrap_or(FileSystem::Raw);
        Ok(Self::with(image, fs))
    }

    /// Open `image` as [`FileSystem::Raw`] whatever it holds: sectors
    /// only, e.g. to repair a damaged disk by hand.
    pub fn open_raw(image: DiskImage) -> Self {
        Self::with(image, FileSystem::Raw)
    }

    fn with(image: DiskImage, fs: FileSystem) -> Self {
        Self {
            image,
            fs,
            overwrite: false,
        }
    }

    /// The file system on the disk ([`FileSystem::Raw`] if none).
    pub fn file_system(&self) -> FileSystem {
        self.fs
    }

    /// A blank, formatted disk, as the drive's `N0:name,id` command makes
    /// one: `name` is up to 16 PETSCII bytes, `id` the two-character disk
    /// ID. Every other sector is zero, and there are no error bytes.
    /// 40-track disks can't be formatted yet ([`Error::Unsupported`]).
    ///
    /// ```
    /// use cbm_disk::{Layout, Volume};
    ///
    /// let vol = Volume::format(Layout::D81, b"BACKUP", *b"42")?;
    /// assert_eq!(vol.blocks_free(), 3160);
    /// assert_eq!(vol.disk_name(), Some(b"BACKUP".to_vec()));
    /// # Ok::<(), cbm_disk::Error>(())
    /// ```
    pub fn format(layout: Layout, name: impl AsRef<[u8]>, id: [u8; 2]) -> Result<Self> {
        let name = name.as_ref();
        check_disk_name(name)?;
        Self::open(cbmdos::format(layout, name, id)?)
    }

    /// Which kind of disk this is.
    pub fn layout(&self) -> Layout {
        self.image.layout()
    }

    /// What this disk can do (see [`Features`]).
    ///
    /// ```
    /// use cbm_disk::{DiskImage, FileSystem, Layout, Volume};
    ///
    /// let vol = Volume::format(Layout::D64, b"", *b"00")?;
    /// assert!(vol.features().files && !vol.features().directories);
    /// assert_eq!(vol.features().max_entries, 144);
    /// // An unformatted disk: sectors only.
    /// let raw = Volume::open(DiskImage::new(Layout::D64))?;
    /// assert_eq!(raw.file_system(), FileSystem::Raw);
    /// assert!(!raw.features().files);
    /// # Ok::<(), cbm_disk::Error>(())
    /// ```
    pub fn features(&self) -> Features {
        self.fs.features(self.layout())
    }

    /// The disk image, to save.
    pub fn image(&self) -> &DiskImage {
        &self.image
    }

    /// Give up the volume, keeping the disk image.
    pub fn into_image(self) -> DiskImage {
        self.image
    }

    /// Sector `sector` of track `track` as it is on the disk.
    pub fn read_sector(&self, track: u8, sector: u8) -> Result<&[u8]> {
        self.image.sector(track, sector)
    }

    /// Write a sector directly, bypassing the file system: for making
    /// custom disks. `bytes` must be 256 long. Nothing checks that the
    /// file system still makes sense afterwards; if it doesn't, file calls
    /// fail with [`Error::Damaged`]. If the disk now holds a different
    /// file system, the volume switches to it.
    pub fn write_sector(&mut self, track: u8, sector: u8, bytes: &[u8]) -> Result<()> {
        self.image.write_sector(track, sector, bytes)?;
        if let Some(fs) = Self::detect(&self.image) {
            self.fs = fs;
        }
        Ok(())
    }

    /// Let [`Volume::write`] replace a file that's already there, instead
    /// of failing with [`Error::AlreadyExists`] (the default). Locked
    /// files are still refused.
    pub fn set_overwrite(&mut self, on: bool) {
        self.overwrite = on;
    }

    /// Is [`Volume::set_overwrite`] on?
    pub fn overwrite(&self) -> bool {
        self.overwrite
    }

    /// Is `name` allowed as a new file or directory name? `Ok`, or
    /// [`Error::BadName`] saying why not. A name is 1 to 16 bytes of any
    /// PETSCII the disk can store; it can't end with a shifted space
    /// (`$A0`), which pads names, so it would be lost.
    ///
    /// ```
    /// use cbm_disk::{Layout, Volume};
    ///
    /// let vol = Volume::format(Layout::D64, b"", *b"00")?;
    /// assert!(vol.check_name("GAME,8,1").is_ok());
    /// assert!(vol.check_name("A NAME LONGER THAN 16").is_err());
    /// # Ok::<(), cbm_disk::Error>(())
    /// ```
    pub fn check_name(&self, name: impl AsRef<[u8]>) -> Result<()> {
        check_name(name.as_ref())
    }

    // -- The disk's header ---------------------------------------------

    fn header(&self) -> (u8, usize, usize, usize) {
        // (track, name at, id at, DOS type at) in sector 0.
        match self.layout() {
            Layout::D81 => (40, 0x04, 0x16, 0x19),
            _ => (18, 0x90, 0xA2, 0xA5),
        }
    }

    /// Bytes of the header sector, if the disk has a CBM header (CBM DOS
    /// and the C128's CP/M disks do; raw ones don't).
    fn header_bytes(&self, at: usize, len: usize) -> Option<Vec<u8>> {
        if self.fs == FileSystem::Raw {
            return None;
        }
        let (t, ..) = self.header();
        let s = self.image.sector(t, 0).expect("on disk");
        Some(s[at..at + len].to_vec())
    }

    /// The disk name as stored (PETSCII, without its `$A0` padding).
    /// `None` on a [`FileSystem::Raw`] disk; CP/M disks have one too.
    pub fn disk_name(&self) -> Option<Vec<u8>> {
        let (_, at, ..) = self.header();
        Some(cbmdos::trim_name(&self.header_bytes(at, 16)?))
    }

    /// The two-byte disk ID (`None` on a raw disk).
    pub fn disk_id(&self) -> Option<[u8; 2]> {
        let (_, _, at, _) = self.header();
        let b = self.header_bytes(at, 2)?;
        Some([b[0], b[1]])
    }

    /// The two-byte DOS type shown after the ID: `2A` on 1541 and 1571
    /// disks, `3D` on 1581 disks (`None` on a raw disk).
    pub fn dos_type(&self) -> Option<[u8; 2]> {
        let (.., at) = self.header();
        let b = self.header_bytes(at, 2)?;
        Some([b[0], b[1]])
    }

    /// The DOS version byte: `$41` (`A`) on 1541 and 1571 disks, `$44`
    /// (`D`) on 1581 disks (`None` on a raw disk). A 1541 refuses to write
    /// to a disk with any other value ("soft" write protection); this
    /// crate only reports it, and the program decides.
    pub fn dos_version(&self) -> Option<u8> {
        Some(self.header_bytes(2, 1)?[0])
    }

    /// Set the disk name: up to 16 PETSCII bytes (CBM DOS only;
    /// [`Error::Unsupported`] otherwise).
    pub fn set_disk_name(&mut self, name: impl AsRef<[u8]>) -> Result<()> {
        self.need_files()?;
        let name = name.as_ref();
        check_disk_name(name)?;
        let (t, at, ..) = self.header();
        self.image.sector_mut(t, 0)?[at..at + 16].copy_from_slice(&cbmdos::padded(name));
        Ok(())
    }

    /// Set the two-byte disk ID (on a 1581 disk, the copies in its BAM
    /// too). CBM DOS only.
    pub fn set_disk_id(&mut self, id: [u8; 2]) -> Result<()> {
        self.need_files()?;
        let (t, _, at, _) = self.header();
        self.image.sector_mut(t, 0)?[at..at + 2].copy_from_slice(&id);
        if self.layout() == Layout::D81 {
            for s in 1..=2 {
                self.image.sector_mut(40, s)?[4..6].copy_from_slice(&id);
            }
        }
        Ok(())
    }

    /// How a 40-track D64 keeps tracks 36–40 in its BAM; `None` on other
    /// disks.
    pub fn extra_tracks(&self) -> Option<ExtraTracks> {
        (self.layout() == Layout::D64_40 && self.fs == FileSystem::CbmDos)
            .then(|| Bam::new(&self.image, &Dir::root(self.layout())).extra())
    }

    /// Is this a GEOS disk (the signature `GEOS format` at `$AD` of the
    /// header sector)?
    pub fn is_geos(&self) -> bool {
        self.fs == FileSystem::CbmDos && check::geos_border(&self.image).is_some()
    }

    // -- Free space ------------------------------------------------------

    /// Blocks free, as the drive's directory listing shows it (the
    /// directory track isn't counted). 0 unless the disk is CBM DOS.
    pub fn blocks_free(&self) -> u32 {
        if self.fs != FileSystem::CbmDos {
            return 0;
        }
        let root = Dir::root(self.layout());
        Bam::new(&self.image, &root).blocks_free(&self.image, &root)
    }

    /// Blocks free in directory `dir` (`""` for the main one; on a 1581,
    /// a subdirectory has its own).
    pub fn dir_blocks_free(&self, dir: impl AsRef<[u8]>) -> Result<u32> {
        let dir = self.resolve_dir(dir.as_ref())?;
        Ok(Bam::new(&self.image, &dir).blocks_free(&self.image, &dir))
    }

    /// Free space in bytes: 254 a block.
    pub fn free_bytes(&self) -> u32 {
        self.blocks_free() * cbmdos::BLOCK_DATA as u32
    }

    // -- Listing and reading ---------------------------------------------

    /// The entries in directory `dir`, in directory order: `""` for the
    /// main one, or e.g. `"GAMES"` on a 1581 disk. Fails with
    /// [`Error::NotFound`] if there's no such directory, or
    /// [`Error::NotADirectory`] if it's a file. A directory whose chain
    /// breaks lists what could be read.
    ///
    /// ```
    /// use cbm_disk::{FileType, Layout, Volume};
    ///
    /// let mut vol = Volume::format(Layout::D64, b"GAMES", *b"01")?;
    /// vol.write("PAC", FileType::Prg, &[0x01, 0x08, 0, 0])?;
    /// for entry in vol.list("")? {
    ///     println!("{:<4} {:16} {}", entry.blocks, entry.name_text(), entry.type_text());
    /// }
    /// assert_eq!(vol.list("")?[0].name, b"PAC");
    /// # Ok::<(), cbm_disk::Error>(())
    /// ```
    pub fn list(&self, dir: impl AsRef<[u8]>) -> Result<Vec<DirEntry>> {
        let dir = self.resolve_dir(dir.as_ref())?;
        let (raws, _) = cbmdos::entries(&self.image, &dir);
        Ok(raws.iter().map(|r| self.dir_entry(&dir, r)).collect())
    }

    /// The entry at `path`, without listing its whole directory.
    pub fn entry(&self, path: impl AsRef<[u8]>) -> Result<DirEntry> {
        let (dir, raw) = self.find(path.as_ref())?;
        Ok(self.dir_entry(&dir, &raw))
    }

    /// Is there a file or directory at `path`?
    pub fn exists(&self, path: impl AsRef<[u8]>) -> bool {
        self.find(path.as_ref()).is_ok()
    }

    /// A file's contents, as its chain of blocks holds them (a PRG's
    /// load address included).
    ///
    /// * A file that was never closed (a splat file) is read as far as
    ///   its chain goes; any other file whose chain breaks gives
    ///   [`Error::Damaged`].
    /// * REL files: the data, records one after another.
    /// * GEOS VLIR files: only the plain chain (the record index) for
    ///   now.
    /// * A 1581 partition that isn't a subdirectory: all its sectors, 256
    ///   bytes each. A subdirectory gives [`Error::IsDirectory`].
    pub fn read(&self, path: impl AsRef<[u8]>) -> Result<Vec<u8>> {
        let (dir, raw) = self.find(path.as_ref())?;
        if raw.file_type() == FileType::Cbm {
            if cbmdos::subdir(&self.image, &dir, &raw).is_some() {
                return Err(Error::IsDirectory(raw.name_text()));
            }
            let run = cbmdos::run(self.layout(), raw.start(), u32::from(raw.blocks()))
                .ok_or_else(|| Error::Damaged(format!("{} runs off the disk", raw.name_text())))?;
            let mut out = Vec::with_capacity(run.len() * SECTOR_SIZE);
            for (t, s) in run {
                out.extend_from_slice(self.image.sector(t, s)?);
            }
            return Ok(out);
        }
        if raw.start().0 == 0 {
            return Ok(Vec::new());
        }
        let chain = cbmdos::follow(&self.image, raw.start());
        if let Some(why) = &chain.broken
            && raw.closed()
        {
            return Err(Error::Damaged(format!("{}: {why}", raw.name_text())));
        }
        Ok(cbmdos::chain_data(&self.image, &chain))
    }

    /// Bytes `range` of a file (e.g. `100..200`); a range running past
    /// the end gives what there is, and one starting past it gives none.
    pub fn read_range(&self, path: impl AsRef<[u8]>, range: Range<usize>) -> Result<Vec<u8>> {
        let data = self.read(path)?;
        let end = range.end.min(data.len());
        let start = range.start.min(end);
        Ok(data[start..end].to_vec())
    }

    // -- Changing files --------------------------------------------------

    /// Write a file of type PRG, SEQ or USR (others give
    /// [`Error::Unsupported`]). If one of that name is already there this
    /// fails with [`Error::AlreadyExists`], unless
    /// [`Volume::set_overwrite`] is on; then it's replaced (its old blocks
    /// freed first, so a file that only fits once the old one is gone
    /// still fits), unless it's locked ([`Error::Locked`]).
    ///
    /// The blocks are chosen as the drive chooses them. An empty file
    /// gets one block holding a carriage return (`$0D`), as the drives
    /// write it, so it reads back as that one byte.
    ///
    /// ```
    /// use cbm_disk::{Error, FileType, Layout, Volume};
    ///
    /// let mut vol = Volume::format(Layout::D64, b"", *b"00")?;
    /// vol.write("NOTES", FileType::Seq, b"FIRST")?;
    /// assert!(matches!(vol.write("NOTES", FileType::Seq, b"SECOND"), Err(Error::AlreadyExists(_))));
    /// vol.set_overwrite(true);
    /// vol.write("NOTES", FileType::Seq, b"SECOND")?;
    /// assert_eq!(vol.read("NOTES")?, b"SECOND");
    /// # Ok::<(), cbm_disk::Error>(())
    /// ```
    pub fn write(
        &mut self,
        path: impl AsRef<[u8]>,
        file_type: FileType,
        data: &[u8],
    ) -> Result<()> {
        if !file_type.writable() {
            return Err(Error::Unsupported(format!(
                "writing {} files",
                file_type.letters()
            )));
        }
        let (dir, name) = self.locate(path.as_ref())?;
        let old = self.in_dir(&dir, &name)?;
        match &old {
            Some(old) => {
                if old.file_type() == FileType::Cbm || !self.overwrite {
                    return Err(Error::AlreadyExists(old.name_text()));
                }
                check_unlocked(old)?;
            }
            None => check_name(&name)?,
        }
        self.change(|image| write_file(image, &dir, &name, old.as_ref(), file_type, data))
    }

    /// Would [`Volume::write`] of `len` bytes to `path` fit? `Ok(false)`
    /// when the disk or directory is too full; other problems (a bad
    /// name, a locked file, ...) are errors, as `write` would give them.
    pub fn fits(&self, path: impl AsRef<[u8]>, len: usize) -> Result<bool> {
        let mut trial = self.clone();
        match trial.write(path, FileType::Prg, &vec![0; len]) {
            Ok(()) => Ok(true),
            Err(Error::DiskFull { .. } | Error::DirectoryFull) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Give a file (or directory) the name `new_name` (a name only: it
    /// stays in its directory). Fails with [`Error::Locked`] if it's
    /// locked, or [`Error::AlreadyExists`] if another entry has that
    /// name.
    pub fn rename(&mut self, path: impl AsRef<[u8]>, new_name: impl AsRef<[u8]>) -> Result<()> {
        let new_name = new_name.as_ref();
        let (dir, raw) = self.find(path.as_ref())?;
        check_unlocked(&raw)?;
        check_name(new_name)?;
        if let Some(other) = self.in_dir(&dir, new_name)?
            && other.slot != raw.slot
        {
            return Err(Error::AlreadyExists(other.name_text()));
        }
        let mut raw = raw;
        raw.bytes[3..19].copy_from_slice(&cbmdos::padded(new_name));
        self.change(|image| {
            need_dir(image, &dir)?;
            cbmdos::write_slot(image, &raw)
        })
    }

    /// Delete ("scratch") a file, or an empty directory, freeing its
    /// blocks. Fails with [`Error::Locked`] if it's locked, or
    /// [`Error::DirectoryNotEmpty`].
    pub fn delete(&mut self, path: impl AsRef<[u8]>) -> Result<()> {
        let (dir, raw) = self.find(path.as_ref())?;
        check_unlocked(&raw)?;
        if let Some(sub) = cbmdos::subdir(&self.image, &dir, &raw)
            && !cbmdos::entries(&self.image, &sub).0.is_empty()
        {
            return Err(Error::DirectoryNotEmpty(raw.name_text()));
        }
        self.change(|image| {
            need_dir(image, &dir)?;
            let scan = check::scan(image);
            let mut taken = scan.taken();
            free_entry(image, &dir, &raw, &scan, &mut taken)?;
            let mut gone = raw.clone();
            gone.bytes[0] = 0;
            cbmdos::write_slot(image, &gone)
        })
    }

    /// Lock a file (or directory), so it can't be written, renamed or
    /// deleted (`<` in a listing).
    pub fn lock(&mut self, path: impl AsRef<[u8]>) -> Result<()> {
        self.set_locked(path.as_ref(), true)
    }

    /// Unlock a file (or directory).
    pub fn unlock(&mut self, path: impl AsRef<[u8]>) -> Result<()> {
        self.set_locked(path.as_ref(), false)
    }

    fn set_locked(&mut self, path: &[u8], locked: bool) -> Result<()> {
        let (dir, mut raw) = self.find(path)?;
        if locked {
            raw.bytes[0] |= 0x40;
        } else {
            raw.bytes[0] &= !0x40;
        }
        self.change(|image| {
            need_dir(image, &dir)?;
            cbmdos::write_slot(image, &raw)
        })
    }

    /// Move a file into directory `dir` (`""` for the main one) on a 1581
    /// disk, keeping its name, type and contents. Each 1581 directory has
    /// its own blocks, so the file is copied there and then deleted
    /// here. Only PRG, SEQ and USR files that aren't GEOS files can be
    /// moved, not directories ([`Error::Unsupported`] otherwise, and on
    /// 1541 and 1571 disks). Fails with [`Error::Locked`] if it's locked,
    /// or [`Error::AlreadyExists`] if `dir` has that name already. Moving
    /// a file to the directory it's in does nothing, but a locked file or
    /// one of a kind that can't be moved is still refused.
    ///
    /// ```
    /// use cbm_disk::{FileType, Layout, Volume};
    ///
    /// let mut vol = Volume::format(Layout::D81, b"", *b"00")?;
    /// vol.make_dir("OLD", 3)?;
    /// vol.write("LETTER", FileType::Seq, b"DEAR COMMODORE")?;
    /// vol.move_to("LETTER", "OLD")?;
    /// assert_eq!(vol.read("OLD/LETTER")?, b"DEAR COMMODORE");
    /// assert!(!vol.exists("LETTER"));
    /// # Ok::<(), cbm_disk::Error>(())
    /// ```
    pub fn move_to(&mut self, path: impl AsRef<[u8]>, dir: impl AsRef<[u8]>) -> Result<()> {
        self.need_dirs()?;
        let (from, raw) = self.find(path.as_ref())?;
        // A file that can't be moved is refused even when it's already
        // in `dir`: locked, or of a kind this can't move.
        check_unlocked(&raw)?;
        let file_type = raw.file_type();
        if !file_type.writable() || raw.is_geos(self.layout()) {
            return Err(Error::Unsupported(if file_type == FileType::Cbm {
                "moving directories or 1581 partitions".into()
            } else if raw.is_geos(self.layout()) {
                "moving GEOS files".into()
            } else {
                format!("moving {} files", file_type.letters())
            }));
        }
        let to = self.resolve_dir(dir.as_ref())?;
        if from == to {
            return Ok(());
        }
        let name = raw.name();
        if let Some(other) = self.in_dir(&to, &name)? {
            return Err(Error::AlreadyExists(other.name_text()));
        }
        let data = self.read(path.as_ref())?;
        let mut trial = self.clone();
        trial.change(|image| {
            write_file(image, &to, &name, None, file_type, &data)?;
            need_dir(image, &from)?;
            let scan = check::scan(image);
            let mut taken = scan.taken();
            free_entry(image, &from, &raw, &scan, &mut taken)?;
            let mut gone = raw.clone();
            gone.bytes[0] = 0;
            cbmdos::write_slot(image, &gone)
        })?;
        self.image = trial.image;
        Ok(())
    }

    // -- Directories (1581 partitions) -----------------------------------

    /// Make an empty directory of `tracks` whole tracks (3 or more; 40
    /// blocks each, one track of which holds its directory) on a 1581
    /// disk, e.g. `"GAMES"` or `"GAMES/OLD"` (its parent must be there).
    /// It goes in the free tracks nearest its parent's directory track;
    /// [`Volume::make_dir_at`] says where. Fails with
    /// [`Error::DiskFull`] if no run of that many free tracks is left,
    /// and with [`Error::Unsupported`] on 1541 and 1571 disks.
    ///
    /// ```
    /// use cbm_disk::{FileType, Layout, Volume};
    ///
    /// let mut vol = Volume::format(Layout::D81, b"", *b"00")?;
    /// vol.make_dir("GAMES", 4)?;
    /// vol.write("GAMES/PAC", FileType::Prg, &[0x01, 0x08])?;
    /// assert!(vol.entry("GAMES")?.is_dir());
    /// assert_eq!(vol.dir_blocks_free("GAMES")?, 120 - 1); // its directory track isn't counted
    /// assert_eq!(vol.blocks_free(), 3160 - 160);
    /// # Ok::<(), cbm_disk::Error>(())
    /// ```
    pub fn make_dir(&mut self, path: impl AsRef<[u8]>, tracks: u8) -> Result<()> {
        self.need_dirs()?;
        need_three(tracks)?;
        let (parent, _) = self.locate(path.as_ref())?;
        let start = self.free_run(&parent, tracks)?;
        self.make_dir_at(path, start, tracks)
    }

    /// Make an empty directory of `tracks` whole tracks starting at track
    /// `track`, as the 1581's `/0:name,` partition command followed by
    /// `N0:name,id` inside it does. Every block of those tracks must be
    /// free, inside the parent directory and off its directory track. The
    /// new directory's header takes its name, and the parent's disk ID.
    pub fn make_dir_at(&mut self, path: impl AsRef<[u8]>, track: u8, tracks: u8) -> Result<()> {
        self.need_dirs()?;
        let (parent, name) = self.locate(path.as_ref())?;
        if let Some(old) = self.in_dir(&parent, &name)? {
            return Err(Error::AlreadyExists(old.name_text()));
        }
        check_name(&name)?;
        need_three(tracks)?;
        let last = u16::from(track) + u16::from(tracks) - 1;
        if track < parent.lo || last > u16::from(parent.hi) {
            return Err(Error::Unsupported(format!(
                "tracks {track}-{last} aren't all inside {}",
                dir_text(&parent)
            )));
        }
        if (track..=last as u8).contains(&parent.track) {
            return Err(Error::Unsupported(format!(
                "a directory can't include track {}, which holds {}",
                parent.track,
                dir_text(&parent)
            )));
        }
        self.change(|image| {
            need_dir(image, &parent)?;
            let scan = check::scan(image);
            let mut taken = scan.taken();
            let bam = Bam::new(image, &parent);
            let layout = image.layout();
            for t in track..=last as u8 {
                for s in 0..40 {
                    let n = layout.index(t, s).expect("on disk");
                    if bam.is_free(image, t, s) != Some(true) || taken[n] {
                        return Err(Error::DiskFull {
                            needed: u32::from(tracks) * 40,
                            free: bam.blocks_free(image, &parent),
                        });
                    }
                }
            }
            let slot = free_slot(image, &bam, &parent, &mut taken)?;
            for t in track..=last as u8 {
                for s in 0..40 {
                    bam.set(image, t, s, false)?;
                }
            }
            let id = {
                let h = image.sector(parent.track, 0)?;
                [h[0x16], h[0x17]]
            };
            let sub = Dir::partition(&parent, &name, track, tracks);
            cbmdos::format_1581(image, &sub, &name, id)?;
            let mut raw = Raw {
                slot,
                bytes: [0; 30],
            };
            raw.bytes[0] = 0x80 | FileType::Cbm.code();
            raw.bytes[1] = track;
            raw.bytes[2] = 0;
            raw.bytes[3..19].copy_from_slice(&cbmdos::padded(&name));
            raw.bytes[28..30].copy_from_slice(&(u16::from(tracks) * 40).to_le_bytes());
            cbmdos::write_slot(image, &raw)
        })
    }

    /// The first track of the run of `tracks` free tracks nearest
    /// `parent`'s directory track.
    fn free_run(&self, parent: &Dir, tracks: u8) -> Result<u8> {
        let scan = check::scan(&self.image);
        let taken = scan.taken();
        let bam = Bam::new(&self.image, parent);
        let layout = self.layout();
        let whole = |t: u8| {
            t != parent.track
                && (0..40).all(|s| {
                    bam.is_free(&self.image, t, s) == Some(true)
                        && !taken[layout.index(t, s).expect("on disk")]
                })
        };
        let mut best: Option<(u8, u8)> = None;
        for start in parent.lo..=parent.hi.saturating_sub(tracks - 1) {
            let end = start + tracks - 1;
            if (start..=end).all(whole) {
                let distance = if end < parent.track {
                    parent.track - end
                } else {
                    start.saturating_sub(parent.track)
                };
                if best.is_none_or(|(d, _)| distance < d) {
                    best = Some((distance, start));
                }
            }
        }
        best.map(|(_, start)| start).ok_or(Error::DiskFull {
            needed: u32::from(tracks) * 40,
            free: bam.blocks_free(&self.image, parent),
        })
    }

    // -- Checking --------------------------------------------------------

    /// What each sector holds, in image order: index `n` is the sector at
    /// [`Layout::position`]`(n)` (track 1 sector 0 first). Where two
    /// things claim a sector it shows the first; [`Volume::validate`]
    /// reports both. CBM DOS only ([`Error::Unsupported`] otherwise).
    ///
    /// ```
    /// use cbm_disk::{FileType, Layout, SectorUse, Volume};
    ///
    /// let mut vol = Volume::format(Layout::D64, b"", *b"00")?;
    /// vol.write("HI", FileType::Seq, b"HELLO")?;
    /// let map = vol.sector_map()?;
    /// let at = |t, s| Layout::D64.index(t, s).unwrap();
    /// assert_eq!(map[at(18, 0)], SectorUse::System); // the BAM
    /// assert_eq!(map[at(18, 1)], SectorUse::Directory(vec![]));
    /// assert_eq!(map[at(17, 0)], SectorUse::File(b"HI".to_vec()));
    /// # Ok::<(), cbm_disk::Error>(())
    /// ```
    pub fn sector_map(&self) -> Result<Vec<SectorUse>> {
        self.need_files()?;
        Ok(check::scan(&self.image).map)
    }

    /// Check the whole disk, changing nothing: every file and directory
    /// can be followed, no block is claimed twice, the BAM marks exactly
    /// the blocks in use, its counts match its bitmaps, and every file
    /// was closed. An empty list means the disk is sound. Fails with
    /// [`Error::Unsupported`] unless the disk is CBM DOS.
    ///
    /// ```
    /// use cbm_disk::{Layout, Volume};
    ///
    /// let vol = Volume::format(Layout::D71, b"", *b"00")?;
    /// for problem in vol.validate()? {
    ///     println!("{problem}"); // e.g. "block 17/3 is marked free but belongs to file GAME"
    /// }
    /// assert!(vol.validate()?.is_empty());
    /// # Ok::<(), cbm_disk::Error>(())
    /// ```
    pub fn validate(&self) -> Result<Vec<Problem>> {
        self.need_files()?;
        Ok(check::scan(&self.image).problems)
    }

    // -- Inside ----------------------------------------------------------

    /// File calls need a CBM DOS disk.
    fn need_files(&self) -> Result<()> {
        match self.fs {
            FileSystem::CbmDos => Ok(()),
            FileSystem::CpM => Err(Error::Unsupported(
                "this is a CP/M disk: only sector access is supported".into(),
            )),
            FileSystem::Raw => Err(Error::Unsupported(
                "no CBM DOS file system was found on this disk: only sector access works".into(),
            )),
        }
    }

    fn need_dirs(&self) -> Result<()> {
        self.need_files()?;
        if self.features().directories {
            Ok(())
        } else {
            Err(Error::Unsupported(format!(
                "directories on a {} disk (only the 1581 has them)",
                self.layout().drive()
            )))
        }
    }

    fn change(&mut self, f: impl FnOnce(&mut DiskImage) -> Result<()>) -> Result<()> {
        let mut work = self.image.clone();
        f(&mut work)?;
        self.image = work;
        Ok(())
    }

    fn dir_entry(&self, dir: &Dir, raw: &Raw) -> DirEntry {
        let layout = self.layout();
        let file_type = raw.file_type();
        let subdir = cbmdos::subdir(&self.image, dir, raw).is_some();
        let (size, damaged) = if file_type == FileType::Cbm {
            let ok = cbmdos::run(layout, raw.start(), u32::from(raw.blocks())).is_some();
            (u32::from(raw.blocks()) * SECTOR_SIZE as u32, !ok)
        } else if raw.start().0 == 0 {
            (0, false)
        } else if raw.is_vlir(layout) {
            let (_, broken) = check::entry_blocks(&self.image, raw);
            let index = self.image.sector(raw.start().0, raw.start().1).ok();
            let mut size = 0;
            if let Some(index) = index {
                for pair in index[2..].chunks(2) {
                    match (pair[0], pair[1]) {
                        (0, 0) => break,
                        (0, _) => {}
                        record => size += cbmdos::follow(&self.image, record).len,
                    }
                }
            }
            (size, broken.is_some())
        } else {
            let chain = cbmdos::follow(&self.image, raw.start());
            (chain.len, chain.broken.is_some())
        };
        let b = &raw.bytes;
        DirEntry {
            name: raw.name(),
            file_type,
            closed: raw.closed(),
            locked: raw.locked(),
            blocks: raw.blocks(),
            size,
            damaged,
            start: raw.start(),
            record_length: (file_type == FileType::Rel).then_some(b[21]),
            geos: raw.is_geos(layout).then(|| Geos {
                file_type: b[22],
                vlir: b[21] == 1,
                info_block: raw.side(),
                date: (
                    if b[23] < 80 {
                        2000 + u16::from(b[23])
                    } else {
                        1900 + u16::from(b[23])
                    },
                    b[24],
                    b[25],
                    b[26],
                    b[27],
                ),
            }),
            directory: subdir,
            raw: raw.bytes,
        }
    }

    /// The entry called `name` in `dir`, if there is one.
    fn in_dir(&self, dir: &Dir, name: &[u8]) -> Result<Option<Raw>> {
        let (raws, _) = cbmdos::entries(&self.image, dir);
        Ok(raws.into_iter().find(|r| r.name() == name))
    }

    /// The directory a path ends in.
    fn resolve_dir(&self, path: &[u8]) -> Result<Dir> {
        self.need_files()?;
        let path = path.strip_suffix(b"/").unwrap_or(path);
        let root = Dir::root(self.layout());
        if path.is_empty() {
            return Ok(root);
        }
        let (dir, name) = self.locate(path)?;
        let raw = self
            .in_dir(&dir, &name)?
            .ok_or_else(|| Error::NotFound(petscii_to_ascii(path)))?;
        cbmdos::subdir(&self.image, &dir, &raw)
            .ok_or_else(|| Error::NotADirectory(petscii_to_ascii(path)))
    }

    /// The directory a path is in, and the name at its end. A `/` splits
    /// the path only where the part before it is a subdirectory, and an
    /// entry whose whole name matches what's left wins.
    fn locate(&self, path: &[u8]) -> Result<(Dir, Vec<u8>)> {
        self.need_files()?;
        let mut dir = Dir::root(self.layout());
        let mut rest = path;
        let mut depth = 0;
        'walk: loop {
            if self.layout() != Layout::D81 || self.in_dir(&dir, rest)?.is_some() {
                break;
            }
            for (i, _) in rest.iter().enumerate().filter(|&(_, &c)| c == b'/') {
                if let Some(raw) = self.in_dir(&dir, &rest[..i])?
                    && let Some(sub) = cbmdos::subdir(&self.image, &dir, &raw)
                {
                    dir = sub;
                    rest = &rest[i + 1..];
                    depth += 1;
                    if depth > 16 {
                        return Err(Error::Damaged("directories nested too deeply".into()));
                    }
                    continue 'walk;
                }
            }
            break;
        }
        Ok((dir, rest.to_vec()))
    }

    /// The existing entry at `path`, and the directory it's in.
    fn find(&self, path: &[u8]) -> Result<(Dir, Raw)> {
        let (dir, name) = self.locate(path)?;
        let raw = self
            .in_dir(&dir, &name)?
            .ok_or_else(|| Error::NotFound(petscii_to_ascii(path)))?;
        Ok((dir, raw))
    }
}

fn dir_text(dir: &Dir) -> String {
    if dir.path.is_empty() {
        "the main directory".into()
    } else {
        format!("directory {}", petscii_to_ascii(&dir.path))
    }
}

fn need_three(tracks: u8) -> Result<()> {
    if tracks < 3 {
        return Err(Error::Unsupported(
            "a 1581 directory needs at least 3 tracks (120 blocks)".into(),
        ));
    }
    Ok(())
}

fn check_name(name: &[u8]) -> Result<()> {
    let text = petscii_to_ascii(name);
    if name.is_empty() {
        return Err(Error::BadName("a name needs at least one character".into()));
    }
    if name.len() > 16 {
        return Err(Error::BadName(format!(
            "{text}: a name can't be longer than 16 characters"
        )));
    }
    if name.last() == Some(&SHIFTED_SPACE) {
        return Err(Error::BadName(format!(
            "{text}: a name can't end with a shifted space ($A0)"
        )));
    }
    Ok(())
}

fn check_disk_name(name: &[u8]) -> Result<()> {
    if name.len() > 16 {
        return Err(Error::BadName(format!(
            "{}: a disk name can't be longer than 16 characters",
            petscii_to_ascii(name)
        )));
    }
    Ok(())
}

fn check_unlocked(raw: &Raw) -> Result<()> {
    if raw.locked() {
        Err(Error::Locked(raw.name_text()))
    } else {
        Ok(())
    }
}

/// Changes to `dir` need its directory chain to be whole.
fn need_dir(image: &DiskImage, dir: &Dir) -> Result<()> {
    match cbmdos::dir_chain(image, dir).broken {
        Some(why) => Err(Error::Damaged(format!(
            "{} can't be followed ({why}), so it can't be changed",
            dir_text(dir)
        ))),
        None => Ok(()),
    }
}

/// An empty slot in `dir`, growing it if every slot is used.
fn free_slot(
    image: &mut DiskImage,
    bam: &Bam,
    dir: &Dir,
    taken: &mut [bool],
) -> Result<cbmdos::Slot> {
    let chain = cbmdos::dir_chain(image, dir);
    if let Some(raw) = cbmdos::slots(image, &chain)
        .into_iter()
        .find(|r| r.type_byte() == 0)
    {
        return Ok(raw.slot);
    }
    let (t, s) = cbmdos::grow_dir(image, bam, dir, &chain, taken)?;
    Ok(cbmdos::Slot {
        track: t,
        sector: s,
        index: 0,
    })
}

/// Free the blocks of an entry in `dir`'s BAM: only blocks nothing else
/// also uses.
fn free_entry(
    image: &mut DiskImage,
    dir: &Dir,
    raw: &Raw,
    scan: &check::Scan,
    taken: &mut [bool],
) -> Result<()> {
    let layout = image.layout();
    let bam = Bam::new(image, dir);
    let shared: HashSet<(u8, u8)> = scan
        .problems
        .iter()
        .filter_map(|p| match p {
            Problem::Shared { track, sector, .. } => Some((*track, *sector)),
            _ => None,
        })
        .collect();
    let (blocks, _) = check::entry_blocks(image, raw);
    for (t, s) in blocks {
        if shared.contains(&(t, s)) || !bam.for_files(dir, t) {
            continue;
        }
        bam.set(image, t, s, true)?;
        if let Some(n) = layout.index(t, s) {
            taken[n] = false;
        }
    }
    Ok(())
}

/// Write a file into `dir`, replacing `old` (an entry in `dir`) if
/// given.
fn write_file(
    image: &mut DiskImage,
    dir: &Dir,
    name: &[u8],
    old: Option<&Raw>,
    file_type: FileType,
    data: &[u8],
) -> Result<()> {
    need_dir(image, dir)?;
    let scan = check::scan(image);
    let mut taken = scan.taken();
    let bam = Bam::new(image, dir);
    let slot = match old {
        Some(old) => {
            free_entry(image, dir, old, &scan, &mut taken)?;
            old.slot
        }
        None => {
            // Check for room first, so a full disk doesn't grow the
            // directory for nothing.
            let needed = data.len().max(1).div_ceil(cbmdos::BLOCK_DATA) as u32;
            let free = Allocator {
                bam,
                dir,
                taken: &mut taken.clone(),
            }
            .available(image);
            if needed > free {
                return Err(Error::DiskFull { needed, free });
            }
            free_slot(image, &bam, dir, &mut taken)?
        }
    };
    let mut alloc = Allocator {
        bam,
        dir,
        taken: &mut taken,
    };
    let (start, blocks) = cbmdos::write_chain(image, &mut alloc, data)?;
    let mut raw = Raw {
        slot,
        bytes: [0; 30],
    };
    raw.bytes[0] = 0x80 | file_type.code();
    raw.bytes[1] = start.0;
    raw.bytes[2] = start.1;
    raw.bytes[3..19].copy_from_slice(&cbmdos::padded(name));
    raw.bytes[28..30].copy_from_slice(&blocks.to_le_bytes());
    cbmdos::write_slot(image, &raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blank(layout: Layout) -> Volume {
        Volume::format(layout, b"TEST", *b"01").unwrap()
    }

    fn names(vol: &Volume, dir: &str) -> Vec<String> {
        vol.list(dir)
            .unwrap()
            .iter()
            .map(DirEntry::name_text)
            .collect()
    }

    #[test]
    fn formats() {
        for (layout, free) in [(Layout::D64, 664), (Layout::D71, 1328), (Layout::D81, 3160)] {
            let vol = blank(layout);
            assert_eq!(vol.blocks_free(), free, "{layout:?}");
            assert_eq!(vol.disk_name(), Some(b"TEST".to_vec()));
            assert_eq!(vol.disk_id(), Some(*b"01"));
            assert!(vol.list("").unwrap().is_empty());
            assert!(vol.validate().unwrap().is_empty(), "{layout:?}");
        }
        assert_eq!(blank(Layout::D64).dos_type(), Some(*b"2A"));
        assert_eq!(blank(Layout::D81).dos_type(), Some(*b"3D"));
        assert_eq!(blank(Layout::D81).dos_version(), Some(b'D'));
        assert!(matches!(
            Volume::format(Layout::D64_40, b"", *b"00"),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            Volume::format(Layout::D64, [b'X'; 17], *b"00"),
            Err(Error::BadName(_))
        ));
    }

    #[test]
    fn write_read_replace() {
        for layout in [Layout::D64, Layout::D71, Layout::D81] {
            let mut vol = blank(layout);
            let data: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
            vol.write("DATA", FileType::Seq, &data).unwrap();
            let e = vol.entry("DATA").unwrap();
            assert_eq!((e.blocks, e.size, e.file_type), (4, 1000, FileType::Seq));
            assert!(e.closed && !e.locked && !e.damaged);
            assert_eq!(vol.read("DATA").unwrap(), data);
            assert_eq!(vol.read_range("DATA", 998..2000).unwrap(), [230, 231]);
            // Exactly a block's worth, and the empty file the drives write.
            vol.write("FULL", FileType::Prg, &[7; 254]).unwrap();
            assert_eq!(vol.entry("FULL").unwrap().blocks, 1);
            vol.write("EMPTY", FileType::Usr, b"").unwrap();
            assert_eq!(vol.read("EMPTY").unwrap(), b"\r");
            assert!(matches!(
                vol.write("DATA", FileType::Seq, b"X"),
                Err(Error::AlreadyExists(_))
            ));
            vol.set_overwrite(true);
            vol.write("DATA", FileType::Prg, b"NEW").unwrap();
            assert_eq!(vol.read("DATA").unwrap(), b"NEW");
            assert_eq!(vol.entry("DATA").unwrap().file_type, FileType::Prg);
            assert_eq!(names(&vol, ""), ["DATA", "FULL", "EMPTY"]);
            assert!(vol.validate().unwrap().is_empty());
            assert!(matches!(
                vol.write("REL", FileType::Rel, b""),
                Err(Error::Unsupported(_))
            ));
        }
    }

    #[test]
    fn replacing_frees_the_old_blocks_first() {
        let mut vol = blank(Layout::D64);
        let all = 664 * 254;
        vol.write("BIG", FileType::Prg, &vec![1; all - 254])
            .unwrap();
        assert_eq!(vol.blocks_free(), 1);
        vol.set_overwrite(true);
        vol.write("BIG", FileType::Prg, &vec![2; all]).unwrap();
        assert_eq!(vol.blocks_free(), 0);
        assert!(vol.validate().unwrap().is_empty());
    }

    #[test]
    fn delete_frees_the_blocks_and_reuses_the_entry() {
        let mut vol = blank(Layout::D64);
        vol.write("ONE", FileType::Prg, &[0; 600]).unwrap();
        vol.write("TWO", FileType::Prg, &[0; 10]).unwrap();
        vol.delete("ONE").unwrap();
        assert_eq!(vol.blocks_free(), 663);
        assert_eq!(names(&vol, ""), ["TWO"]);
        vol.write("THREE", FileType::Seq, b"3").unwrap();
        assert_eq!(names(&vol, ""), ["THREE", "TWO"]);
        assert!(matches!(vol.delete("ONE"), Err(Error::NotFound(_))));
        assert!(vol.validate().unwrap().is_empty());
    }

    #[test]
    fn names_are_checked_but_anything_storable_goes() {
        let mut vol = blank(Layout::D64);
        for odd in [
            &b"GAME,8,1"[..],
            b"A:B=C*?\"",
            &[0x00, 0xFF, 0xA0, 0x41],
            b"A/B",
        ] {
            vol.write(odd, FileType::Prg, b"X").unwrap();
            assert_eq!(vol.read(odd).unwrap(), b"X");
        }
        for bad in [&b""[..], b"SEVENTEEN CHARS!!", &[b'A', 0xA0]] {
            assert!(
                matches!(vol.check_name(bad), Err(Error::BadName(_))),
                "{bad:?}"
            );
        }
        assert!(vol.check_name("SIXTEEN CHARS!!!").is_ok());
    }

    #[test]
    fn rename_lock_and_unlock() {
        let mut vol = blank(Layout::D64);
        vol.write("OLD", FileType::Prg, b"X").unwrap();
        vol.write("OTHER", FileType::Prg, b"Y").unwrap();
        vol.rename("OLD", "NEW").unwrap();
        assert_eq!(vol.read("NEW").unwrap(), b"X");
        assert!(matches!(
            vol.rename("NEW", "OTHER"),
            Err(Error::AlreadyExists(_))
        ));
        vol.rename("NEW", "NEW").unwrap();
        vol.lock("NEW").unwrap();
        let e = vol.entry("NEW").unwrap();
        assert!(e.locked);
        assert_eq!(e.type_text(), "PRG<");
        vol.set_overwrite(true);
        for result in [
            vol.write("NEW", FileType::Prg, b"Z"),
            vol.rename("NEW", "X"),
            vol.delete("NEW"),
        ] {
            assert!(matches!(result, Err(Error::Locked(_))));
        }
        vol.unlock("NEW").unwrap();
        vol.delete("NEW").unwrap();
    }

    #[test]
    fn a_full_disk_leaves_everything_as_it_was() {
        let mut vol = blank(Layout::D64);
        vol.write("A", FileType::Prg, &[0; 254 * 600]).unwrap();
        let before = vol.image().clone();
        let err = vol.write("B", FileType::Prg, &[0; 254 * 65]).unwrap_err();
        assert_eq!(
            err,
            Error::DiskFull {
                needed: 65,
                free: 64
            }
        );
        assert_eq!(vol.image(), &before);
        assert!(vol.fits("B", 254 * 64).unwrap());
        assert!(!vol.fits("B", 254 * 65).unwrap());
    }

    #[test]
    fn the_directory_holds_144_or_296() {
        for (layout, max) in [(Layout::D64, 144), (Layout::D81, 296)] {
            let mut vol = blank(layout);
            for i in 0..max {
                vol.write(format!("F{i}"), FileType::Seq, b"X").unwrap();
            }
            assert_eq!(
                vol.write("ONE MORE", FileType::Seq, b"X"),
                Err(Error::DirectoryFull)
            );
            assert_eq!(vol.features().max_entries, max);
            assert!(vol.validate().unwrap().is_empty());
        }
    }

    #[test]
    fn d71_uses_both_sides_and_both_bams() {
        let mut vol = blank(Layout::D71);
        vol.write("BIG", FileType::Prg, &vec![9; 254 * 1000])
            .unwrap();
        assert_eq!(vol.blocks_free(), 328);
        assert!(vol.validate().unwrap().is_empty());
        let map = vol.sector_map().unwrap();
        let at = |t, s| Layout::D71.index(t, s).unwrap();
        assert_eq!(map[at(53, 0)], SectorUse::System);
        assert_eq!(map[at(53, 1)], SectorUse::Reserved);
        assert_eq!(map[at(36, 0)], SectorUse::File(b"BIG".to_vec()));
        assert_eq!(vol.read("BIG").unwrap().len(), 254 * 1000);
    }

    #[test]
    fn d81_directories() {
        let mut vol = blank(Layout::D81);
        assert!(matches!(
            blank(Layout::D64).make_dir("X", 3),
            Err(Error::Unsupported(_))
        ));
        vol.make_dir("GAMES", 6).unwrap();
        // Its own directory track leaves 5 tracks, so 3 fit inside.
        vol.make_dir("GAMES/OLD", 3).unwrap();
        assert!(matches!(
            vol.make_dir("GAMES/BIG", 3),
            Err(Error::DiskFull { .. })
        ));
        assert!(matches!(
            vol.make_dir("GAMES", 3),
            Err(Error::AlreadyExists(_))
        ));
        assert!(matches!(
            vol.make_dir("TINY", 2),
            Err(Error::Unsupported(_))
        ));
        let games = vol.entry("GAMES").unwrap();
        assert!(games.is_dir() && games.file_type == FileType::Cbm);
        assert_eq!(games.blocks, 240);
        vol.write("GAMES/OLD/PAC", FileType::Prg, b"PAC").unwrap();
        vol.write("GAMES/NEW", FileType::Prg, b"NEW").unwrap();
        vol.write("TOP", FileType::Prg, b"TOP").unwrap();
        assert_eq!(names(&vol, ""), ["GAMES", "TOP"]);
        assert_eq!(names(&vol, "GAMES"), ["OLD", "NEW"]);
        assert_eq!(names(&vol, "GAMES/OLD/"), ["PAC"]);
        assert_eq!(vol.read("GAMES/OLD/PAC").unwrap(), b"PAC");
        assert_eq!(vol.dir_blocks_free("GAMES/OLD").unwrap(), 79);
        assert!(matches!(vol.read("GAMES"), Err(Error::IsDirectory(_))));
        assert!(matches!(vol.list("TOP"), Err(Error::NotADirectory(_))));
        assert!(matches!(vol.list("NONE"), Err(Error::NotFound(_))));
        // A name with `/` where no directory matches is just a name.
        vol.write("TOP/X", FileType::Seq, b"SLASH").unwrap();
        assert_eq!(vol.read("TOP/X").unwrap(), b"SLASH");
        assert!(
            vol.validate().unwrap().is_empty(),
            "{:?}",
            vol.validate().unwrap()
        );

        // A locked file can't be moved, not even to where it already is.
        vol.lock("TOP").unwrap();
        for target in ["", "GAMES/OLD"] {
            assert!(matches!(vol.move_to("TOP", target), Err(Error::Locked(_))));
        }
        vol.unlock("TOP").unwrap();
        vol.move_to("TOP", "").unwrap(); // already there: nothing to do
        vol.move_to("TOP", "GAMES/OLD").unwrap();
        assert_eq!(names(&vol, "GAMES/OLD"), ["PAC", "TOP"]);
        // Kinds it can't move are refused even where they already are.
        for (path, target) in [("GAMES/OLD", "GAMES"), ("GAMES/OLD", "")] {
            assert!(matches!(
                vol.move_to(path, target),
                Err(Error::Unsupported(_))
            ));
        }
        assert!(matches!(
            vol.delete("GAMES/OLD"),
            Err(Error::DirectoryNotEmpty(_))
        ));
        vol.delete("GAMES/OLD/PAC").unwrap();
        vol.delete("GAMES/OLD/TOP").unwrap();
        vol.delete("GAMES/OLD").unwrap();
        assert_eq!(vol.dir_blocks_free("GAMES").unwrap(), 200 - 1);
        vol.delete("GAMES/NEW").unwrap();
        vol.delete("GAMES").unwrap();
        assert_eq!(vol.blocks_free(), 3160 - 1);
        assert!(vol.validate().unwrap().is_empty());
    }

    #[test]
    fn rel_files_cant_be_moved_even_where_they_are() {
        let mut vol = blank(Layout::D81);
        vol.make_dir("SUB", 3).unwrap();
        vol.write("DATA", FileType::Seq, b"X").unwrap();
        // Make it a REL file in place: its entry is the first at 40/3.
        let mut dir = vol.read_sector(40, 3).unwrap().to_vec();
        let slot = dir[2..]
            .chunks(32)
            .position(|e| e[3..7] == *b"DATA")
            .unwrap();
        dir[2 + 32 * slot] = 0x80 | FileType::Rel.code();
        vol.write_sector(40, 3, &dir).unwrap();
        for target in ["", "SUB"] {
            let err = vol.move_to("DATA", target).unwrap_err();
            assert_eq!(err, Error::Unsupported("moving REL files".into()));
        }
    }

    #[test]
    fn make_dir_at_needs_free_whole_tracks() {
        let mut vol = blank(Layout::D81);
        vol.write("FILE", FileType::Prg, b"X").unwrap(); // on track 39
        assert!(matches!(
            vol.make_dir_at("A", 37, 3),
            Err(Error::DiskFull { .. })
        ));
        assert!(matches!(
            vol.make_dir_at("A", 39, 3),
            Err(Error::Unsupported(_))
        ));
        vol.make_dir_at("A", 10, 3).unwrap();
        assert_eq!(vol.entry("A").unwrap().start, (10, 0));
        assert!(vol.validate().unwrap().is_empty());
    }

    #[test]
    fn validate_finds_each_kind_of_problem() {
        let mut vol = blank(Layout::D64);
        vol.write("A", FileType::Prg, &[0; 300]).unwrap();
        let start = vol.entry("A").unwrap().start;
        let bam = Bam::new(vol.image(), &Dir::root(Layout::D64));
        let mut image = vol.image().clone();
        // A block of A marked free, a lost block, a wrong count.
        bam.set(&mut image, start.0, start.1, true).unwrap();
        bam.set(&mut image, 1, 0, false).unwrap();
        image.sector_mut(18, 0).unwrap()[4 * 2] = 5;
        let vol = Volume::open(image).unwrap();
        let problems = vol.validate().unwrap();
        assert!(problems.contains(&Problem::MarkedFree {
            track: start.0,
            sector: start.1,
            owner: SectorUse::File(b"A".to_vec())
        }));
        assert!(problems.contains(&Problem::Unowned {
            track: 1,
            sector: 0
        }));
        assert!(
            problems
                .iter()
                .any(|p| matches!(p, Problem::FreeCount { track: 2, .. }))
        );
        for p in &problems {
            assert!(!p.to_string().is_empty());
        }
    }

    #[test]
    fn damaged_and_splat_files() {
        let mut vol = blank(Layout::D64);
        vol.write("GOOD", FileType::Seq, &[1; 600]).unwrap();
        vol.write("SPLAT", FileType::Seq, &[2; 600]).unwrap();
        let good = vol.entry("GOOD").unwrap().start;
        let splat = vol.entry("SPLAT").unwrap().start;
        let mut image = vol.image().clone();
        // Point both chains off the disk after their first block.
        image.sector_mut(good.0, good.1).unwrap()[0] = 99;
        image.sector_mut(splat.0, splat.1).unwrap()[0] = 99;
        image.sector_mut(18, 1).unwrap()[0x22] = 0x01; // SPLAT's type: open SEQ
        let vol = Volume::open(image).unwrap();
        assert!(matches!(vol.read("GOOD"), Err(Error::Damaged(_))));
        assert_eq!(vol.read("SPLAT").unwrap(), [2; 254]);
        let e = vol.entry("SPLAT").unwrap();
        assert!(!e.closed && e.damaged);
        assert_eq!(e.type_text(), "*SEQ");
        let problems = vol.validate().unwrap();
        assert!(problems.contains(&Problem::Unclosed {
            path: b"SPLAT".to_vec()
        }));
        assert!(
            problems
                .iter()
                .any(|p| matches!(p, Problem::Damaged { .. }))
        );
    }

    #[test]
    fn a_broken_directory_refuses_changes() {
        let mut vol = blank(Layout::D64);
        vol.write("A", FileType::Prg, b"X").unwrap();
        vol.write_sector(18, 1, &{
            let mut s = vol.read_sector(18, 1).unwrap().to_vec();
            s[0] = 99; // a link off the disk
            s
        })
        .unwrap();
        assert_eq!(names(&vol, ""), ["A"]);
        assert!(matches!(
            vol.write("B", FileType::Prg, b"Y"),
            Err(Error::Damaged(_))
        ));
        assert!(matches!(vol.delete("A"), Err(Error::Damaged(_))));
    }

    #[test]
    fn header_and_features() {
        let mut vol = blank(Layout::D81);
        vol.set_disk_name("NEW NAME").unwrap();
        vol.set_disk_id(*b"ZZ").unwrap();
        assert_eq!(vol.disk_name(), Some(b"NEW NAME".to_vec()));
        assert_eq!(vol.disk_id(), Some(*b"ZZ"));
        assert_eq!(&vol.read_sector(40, 1).unwrap()[4..6], b"ZZ");
        assert!(vol.features().directories && vol.features().locking);
        assert!(!vol.is_geos());
        assert_eq!(vol.extra_tracks(), None);
        assert_eq!(vol.free_bytes(), 3160 * 254);
    }

    #[test]
    fn forty_tracks_are_read_but_kept_off_unless_the_bam_covers_them() {
        let mut image = cbmdos::format(Layout::D64, b"FORTY", *b"40")
            .unwrap()
            .to_bytes();
        image.resize(Layout::D64_40.image_len(), 0);
        let mut vol = Volume::open(DiskImage::from_bytes(&image).unwrap()).unwrap();
        assert_eq!(vol.extra_tracks(), Some(ExtraTracks::Uncovered));
        assert_eq!(vol.blocks_free(), 664);
        vol.write("ALL", FileType::Prg, &vec![0; 664 * 254])
            .unwrap();
        assert!(matches!(
            vol.write("MORE", FileType::Prg, b"X"),
            Err(Error::DiskFull { .. })
        ));
        // SpeedDOS-style BAM for tracks 36-40: they're used too.
        let mut image = cbmdos::format(Layout::D64, b"FORTY", *b"40")
            .unwrap()
            .to_bytes();
        image.resize(Layout::D64_40.image_len(), 0);
        for i in 0..5 {
            let at = 0x16500 + 0xC0 + 4 * i;
            image[at..at + 4].copy_from_slice(&[17, 0xFF, 0xFF, 0x01]);
        }
        let mut vol = Volume::open(DiskImage::from_bytes(&image).unwrap()).unwrap();
        assert_eq!(vol.extra_tracks(), Some(ExtraTracks::SpeedDos));
        assert_eq!(vol.blocks_free(), 664 + 85);
        vol.write("ALL", FileType::Prg, &vec![0; 749 * 254])
            .unwrap();
        assert!(vol.validate().unwrap().is_empty());
    }

    #[test]
    fn detects_cbm_dos_raw_and_cpm() {
        for layout in [Layout::D64, Layout::D71, Layout::D81] {
            let vol = blank(layout);
            assert_eq!(Volume::detect(vol.image()), Some(FileSystem::CbmDos));
            assert_eq!(vol.file_system(), FileSystem::CbmDos);
            // Unformatted: raw, sectors only.
            let raw = Volume::open(DiskImage::new(layout)).unwrap();
            assert_eq!(Volume::detect(raw.image()), None);
            assert_eq!(raw.file_system(), FileSystem::Raw);
            assert!(!raw.features().files);
            assert_eq!(raw.blocks_free(), 0);
            assert_eq!(raw.disk_name(), None);
            for result in [
                raw.list("").map(|_| ()),
                raw.read("X").map(|_| ()),
                raw.validate().map(|_| ()),
                raw.sector_map().map(|_| ()),
            ] {
                assert!(matches!(result, Err(Error::Unsupported(_))));
            }
            assert!(!raw.exists("X"));
            assert!(raw.read_sector(1, 0).is_ok());
        }
        // open_raw ignores what's there.
        let forced = Volume::open_raw(blank(Layout::D64).into_image());
        assert_eq!(forced.file_system(), FileSystem::Raw);
        assert!(matches!(
            forced.clone().write("X", FileType::Prg, b""),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn a_damaged_header_is_still_cbm_dos() {
        // The header's link is garbage, but 18/1 is a directory sector:
        // the drive starts there anyway (as on copy-protected disks).
        let mut vol = blank(Layout::D64);
        vol.write("GAME", FileType::Prg, b"X").unwrap();
        let mut image = vol.into_image();
        image.sector_mut(18, 0).unwrap()[..3].copy_from_slice(&[0x00, 0xA0, 0x28]);
        let vol = Volume::open(image).unwrap();
        assert_eq!(vol.file_system(), FileSystem::CbmDos);
        assert_eq!(vol.read("GAME").unwrap(), b"X");
    }

    #[test]
    fn c128_cpm_disks_are_recognised() {
        // The C128 CP/M signature: `CBM` at 1/0, the header linked to 1/5.
        let mut image = blank(Layout::D71).into_image();
        image.sector_mut(1, 0).unwrap()[..3].copy_from_slice(b"CBM");
        image.sector_mut(18, 0).unwrap()[..2].copy_from_slice(&[1, 5]);
        let vol = Volume::open(image).unwrap();
        assert_eq!(vol.file_system(), FileSystem::CpM);
        assert_eq!(vol.file_system().name(), "CP/M");
        assert_eq!(vol.disk_name(), Some(b"TEST".to_vec()));
        let err = vol.list("").unwrap_err();
        assert!(err.to_string().contains("CP/M"), "{err}");
        // A C128 boot sector alone doesn't make a CP/M disk.
        let mut image = blank(Layout::D64).into_image();
        image.sector_mut(1, 0).unwrap()[..3].copy_from_slice(b"CBM");
        assert_eq!(Volume::detect(&image), Some(FileSystem::CbmDos));
    }

    #[test]
    fn writing_sectors_can_change_the_file_system() {
        let mut vol = Volume::open(DiskImage::new(Layout::D64)).unwrap();
        assert_eq!(vol.file_system(), FileSystem::Raw);
        let blank = blank(Layout::D64);
        for s in 0..2 {
            vol.write_sector(18, s, blank.read_sector(18, s).unwrap())
                .unwrap();
        }
        assert_eq!(vol.file_system(), FileSystem::CbmDos);
        assert_eq!(vol.blocks_free(), 664);
    }

    #[test]
    fn features_without_a_disk() {
        assert!(FileSystem::CbmDos.features(Layout::D81).directories);
        assert!(!FileSystem::CbmDos.features(Layout::D71).directories);
        assert!(!FileSystem::CpM.features(Layout::D81).files);
    }
}
