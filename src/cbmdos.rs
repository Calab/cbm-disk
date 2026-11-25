//! CBM DOS itself: the BAM, directory entries, chains of blocks, the way
//! the drives choose free blocks, and formatting. [`crate::Volume`] puts
//! the calls a program uses on top of this.

use crate::error::{Error, Result};
use crate::image::{DiskImage, Layout, SECTOR_SIZE};
use crate::petscii::{SHIFTED_SPACE, petscii_to_ascii};

/// Data bytes in a block: 256 less the 2-byte link to the next block.
pub const BLOCK_DATA: usize = SECTOR_SIZE - 2;

/// How a 40-track D64 keeps the free blocks of tracks 36–40, if it does,
/// from [`crate::Volume::extra_tracks`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExtraTracks {
    /// Not at all (or in no way this crate recognises): tracks 36–40 are
    /// outside the BAM, so nothing new is written there.
    Uncovered,
    /// SpeedDOS: four bytes a track at `$C0`–`$D3` of 18/0.
    SpeedDos,
    /// DolphinDOS: four bytes a track at `$AC`–`$BF` of 18/0.
    DolphinDos,
}

/// The kind of a file, from the low bits of its directory entry's type
/// byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FileType {
    /// `DEL`: an entry DOS lists but can't open (often directory art).
    Del,
    /// `SEQ`: a sequential data file, e.g. text.
    Seq,
    /// `PRG`: a program; its first two bytes are the load address.
    Prg,
    /// `USR`: like SEQ, for the user's own purposes (GEOS uses it).
    Usr,
    /// `REL`: a relative file of fixed-length records (read only here).
    Rel,
    /// `CBM`: a 1581 partition, which may be a subdirectory.
    Cbm,
    /// A type number DOS doesn't define (6 or 7).
    Other(u8),
}

impl FileType {
    /// The type for the low 3 bits of a type byte.
    pub fn from_code(code: u8) -> Self {
        match code & 0x07 {
            0 => FileType::Del,
            1 => FileType::Seq,
            2 => FileType::Prg,
            3 => FileType::Usr,
            4 => FileType::Rel,
            5 => FileType::Cbm,
            n => FileType::Other(n),
        }
    }

    /// The type number stored in the directory (0–7).
    pub fn code(self) -> u8 {
        match self {
            FileType::Del => 0,
            FileType::Seq => 1,
            FileType::Prg => 2,
            FileType::Usr => 3,
            FileType::Rel => 4,
            FileType::Cbm => 5,
            FileType::Other(n) => n & 0x07,
        }
    }

    /// The three letters a directory listing shows: `"PRG"`, `"SEQ"`, ...
    /// (`"???"` for an undefined type).
    pub fn letters(self) -> &'static str {
        match self {
            FileType::Del => "DEL",
            FileType::Seq => "SEQ",
            FileType::Prg => "PRG",
            FileType::Usr => "USR",
            FileType::Rel => "REL",
            FileType::Cbm => "CBM",
            FileType::Other(_) => "???",
        }
    }

    /// Can [`crate::Volume::write`] make a file of this type? PRG, SEQ
    /// and USR only.
    pub fn writable(self) -> bool {
        matches!(self, FileType::Prg | FileType::Seq | FileType::Usr)
    }
}

/// The file systems a [`crate::Volume`] can find on a disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FileSystem {
    /// CBM DOS, as the 1541, 1571 and 1581 drives write it (GEOS disks
    /// included): every call works.
    CbmDos,
    /// CP/M (the C128's CP/M Plus, on 1541, 1571 or 1581 disks):
    /// recognised but not supported, so only the sector calls work, and
    /// the file calls give [`Error::Unsupported`]. (C64 CP/M disks are
    /// ordinary CBM DOS disks holding a `CPM` program, so they're
    /// [`FileSystem::CbmDos`].)
    CpM,
    /// No file system this crate knows (or none at all, e.g. an
    /// unformatted disk): only the sector calls work; file calls give
    /// [`Error::Unsupported`].
    Raw,
}

impl FileSystem {
    /// Its name, to show: `"CBM DOS"`, `"CP/M"` or `"raw"`.
    pub fn name(self) -> &'static str {
        match self {
            FileSystem::CbmDos => "CBM DOS",
            FileSystem::CpM => "CP/M",
            FileSystem::Raw => "raw",
        }
    }
}

/// Which file system `image` holds, if any (see [`crate::Volume::detect`]).
/// Only the header and first directory sector are looked at, so a CBM
/// disk that's damaged elsewhere is still CBM DOS. The DOS version and
/// DOS type bytes aren't used: copy protection and directory art change
/// them, and the drives ignore them when reading.
pub(crate) fn detect(image: &DiskImage) -> Option<FileSystem> {
    let layout = image.layout();
    let root = Dir::root(layout);
    let header = image.sector(root.track, 0).ok()?;
    let link = (header[0], header[1]);
    // The C128's CP/M Plus: its boot sector at 1/0 (which the CP/M BIOS
    // checks for `CBM`), and the header pointing at 1/5, not 18/1.
    let boot = image.sector(1, 0).ok()?;
    if layout != Layout::D81 && boot[..3] == *b"CBM" && link == (1, 5) {
        return Some(FileSystem::CpM);
    }
    let first = image.sector(root.first.0, root.first.1).ok()?;
    let looks_like_dir = (first[0] == 0 && first[1] == 0xFF)
        || (first[0] == root.track && layout.index(first[0], first[1]).is_some());
    if link != root.first && !looks_like_dir {
        return None;
    }
    // A 1581 CP/M disk: the whole disk is marked used, and the CBM
    // directory holds just this one entry.
    if layout == Layout::D81 {
        let (list, _) = entries(image, &root);
        if let [only] = &list[..]
            && only.name() == b"COPYRIGHT CBM 86"
            && only.file_type() == FileType::Usr
            && Bam::new(image, &root).blocks_free(image, &root) == 0
        {
            return Some(FileSystem::CpM);
        }
    }
    Some(FileSystem::CbmDos)
}

// ---------------------------------------------------------------------
// Directories

/// A directory: the main one, or a 1581 partition used as one. Where its
/// header, BAM and entries are, and the tracks it may give to files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Dir {
    /// The track holding its header, BAM (on a 1581) and entries.
    pub track: u8,
    /// Its first directory sector.
    pub first: (u8, u8),
    /// The tracks it covers, `lo..=hi`.
    pub lo: u8,
    pub hi: u8,
    /// Its path (PETSCII, `/` between names; empty for the main one).
    pub path: Vec<u8>,
}

impl Dir {
    pub fn root(layout: Layout) -> Self {
        let track = layout.directory_track();
        let first = if layout == Layout::D81 { 3 } else { 1 };
        Dir {
            track,
            first: (track, first),
            lo: 1,
            hi: layout.tracks(),
            path: Vec::new(),
        }
    }

    /// A 1581 partition starting at `track` with `tracks` tracks, inside
    /// `parent`.
    pub fn partition(parent: &Dir, name: &[u8], track: u8, tracks: u8) -> Self {
        let path = if parent.path.is_empty() {
            name.to_vec()
        } else {
            [&parent.path[..], b"/", name].concat()
        };
        Dir {
            track,
            first: (track, 3),
            lo: track,
            hi: track + tracks - 1,
            path,
        }
    }

    /// The path of an entry called `name` in this directory.
    pub fn child(&self, name: &[u8]) -> Vec<u8> {
        if self.path.is_empty() {
            name.to_vec()
        } else {
            [&self.path[..], b"/", name].concat()
        }
    }
}

/// Where an entry is: its directory sector and slot (0–7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Slot {
    pub track: u8,
    pub sector: u8,
    pub index: u8,
}

/// One directory entry as stored: 30 bytes (the 2 before it are the
/// sector link in slot 0, and unused in the others).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Raw {
    pub slot: Slot,
    pub bytes: [u8; 30],
}

impl Raw {
    pub fn type_byte(&self) -> u8 {
        self.bytes[0]
    }
    pub fn file_type(&self) -> FileType {
        FileType::from_code(self.type_byte())
    }
    pub fn closed(&self) -> bool {
        self.type_byte() & 0x80 != 0
    }
    pub fn locked(&self) -> bool {
        self.type_byte() & 0x40 != 0
    }
    pub fn start(&self) -> (u8, u8) {
        (self.bytes[1], self.bytes[2])
    }
    /// The name, without the `$A0` padding at the end.
    pub fn name(&self) -> Vec<u8> {
        trim_name(&self.bytes[3..19])
    }
    pub fn name_text(&self) -> String {
        petscii_to_ascii(&self.name())
    }
    /// REL: side sector; GEOS: info block.
    pub fn side(&self) -> (u8, u8) {
        (self.bytes[19], self.bytes[20])
    }
    pub fn blocks(&self) -> u16 {
        u16::from_le_bytes([self.bytes[28], self.bytes[29]])
    }
    /// A GEOS file: not REL or CBM, with an info block.
    pub fn is_geos(&self, layout: Layout) -> bool {
        !matches!(self.file_type(), FileType::Rel | FileType::Cbm)
            && layout.index(self.bytes[19], self.bytes[20]).is_some()
            && self.bytes[21] <= 1
    }
    pub fn is_vlir(&self, layout: Layout) -> bool {
        self.is_geos(layout) && self.bytes[21] == 1
    }
}

/// `name` without the `$A0`s padding its end.
pub(crate) fn trim_name(name: &[u8]) -> Vec<u8> {
    let end = name
        .iter()
        .rposition(|&c| c != SHIFTED_SPACE)
        .map_or(0, |i| i + 1);
    name[..end].to_vec()
}

/// `name` padded with `$A0` to 16 bytes.
pub(crate) fn padded(name: &[u8]) -> [u8; 16] {
    let mut out = [SHIFTED_SPACE; 16];
    out[..name.len()].copy_from_slice(name);
    out
}

/// The sectors of a directory, in order, and whether the chain ends
/// properly.
pub(crate) struct DirChain {
    pub sectors: Vec<(u8, u8)>,
    pub broken: Option<String>,
}

pub(crate) fn dir_chain(image: &DiskImage, dir: &Dir) -> DirChain {
    let layout = image.layout();
    let mut seen = vec![false; layout.sector_count() as usize];
    let mut sectors = Vec::new();
    let (mut t, mut s) = dir.first;
    loop {
        let Some(n) = layout.index(t, s) else {
            return DirChain {
                sectors,
                broken: Some(format!("it leads to track {t} sector {s}, off the disk")),
            };
        };
        if seen[n] {
            return DirChain {
                sectors,
                broken: Some(format!("it goes round in a loop at {t}/{s}")),
            };
        }
        seen[n] = true;
        sectors.push((t, s));
        let block = image.sector(t, s).expect("checked");
        if block[0] == 0 {
            return DirChain {
                sectors,
                broken: None,
            };
        }
        (t, s) = (block[0], block[1]);
    }
}

/// Every slot of a directory, used or not, in order.
pub(crate) fn slots(image: &DiskImage, chain: &DirChain) -> Vec<Raw> {
    let mut out = Vec::new();
    for &(t, s) in &chain.sectors {
        let block = image.sector(t, s).expect("checked");
        for i in 0..8u8 {
            let at = usize::from(i) * 32 + 2;
            let mut bytes = [0; 30];
            bytes.copy_from_slice(&block[at..at + 30]);
            out.push(Raw {
                slot: Slot {
                    track: t,
                    sector: s,
                    index: i,
                },
                bytes,
            });
        }
    }
    out
}

/// The entries DOS lists: slots whose type byte isn't 0.
pub(crate) fn entries(image: &DiskImage, dir: &Dir) -> (Vec<Raw>, Option<String>) {
    let chain = dir_chain(image, dir);
    let list = slots(image, &chain)
        .into_iter()
        .filter(|r| r.type_byte() != 0)
        .collect();
    (list, chain.broken)
}

pub(crate) fn write_slot(image: &mut DiskImage, raw: &Raw) -> Result<()> {
    let at = usize::from(raw.slot.index) * 32 + 2;
    image.sector_mut(raw.slot.track, raw.slot.sector)?[at..at + 30].copy_from_slice(&raw.bytes);
    Ok(())
}

/// A 1581 partition that's laid out as a subdirectory: at sector 0, a
/// whole number of tracks (at least 3), all inside `parent` and off its
/// directory track, with a 1581 header at its start.
pub(crate) fn subdir(image: &DiskImage, parent: &Dir, raw: &Raw) -> Option<Dir> {
    if image.layout() != Layout::D81 || raw.file_type() != FileType::Cbm {
        return None;
    }
    let (t, s) = raw.start();
    let blocks = raw.blocks();
    if s != 0 || blocks < 120 || !blocks.is_multiple_of(40) {
        return None;
    }
    let tracks = (blocks / 40) as u8;
    let last = u16::from(t) + u16::from(tracks) - 1;
    if t < parent.lo || last > u16::from(parent.hi) || (t..=last as u8).contains(&parent.track) {
        return None;
    }
    let header = image.sector(t, 0).ok()?;
    if header[2] != b'D' {
        return None;
    }
    Some(Dir::partition(parent, &raw.name(), t, tracks))
}

// ---------------------------------------------------------------------
// Chains of blocks

/// A chain of blocks followed from its start.
#[derive(Debug, Clone)]
pub(crate) struct Chain {
    pub blocks: Vec<(u8, u8)>,
    /// The data bytes in it.
    pub len: u32,
    /// Why it couldn't be followed to its end, if it couldn't.
    pub broken: Option<String>,
}

pub(crate) fn follow(image: &DiskImage, start: (u8, u8)) -> Chain {
    let layout = image.layout();
    let mut seen = vec![false; layout.sector_count() as usize];
    let mut chain = Chain {
        blocks: Vec::new(),
        len: 0,
        broken: None,
    };
    let (mut t, mut s) = start;
    loop {
        let Some(n) = layout.index(t, s) else {
            chain.broken = Some(format!(
                "a link leads to track {t} sector {s}, off the disk"
            ));
            return chain;
        };
        if seen[n] {
            chain.broken = Some(format!("its blocks go round in a loop at {t}/{s}"));
            return chain;
        }
        seen[n] = true;
        chain.blocks.push((t, s));
        let block = image.sector(t, s).expect("checked");
        if block[0] == 0 {
            // The last block: byte 1 is the index of its last data byte.
            chain.len += u32::from(block[1].max(1)) - 1;
            return chain;
        }
        chain.len += BLOCK_DATA as u32;
        (t, s) = (block[0], block[1]);
    }
}

/// The data in a chain's blocks, as far as it goes.
pub(crate) fn chain_data(image: &DiskImage, chain: &Chain) -> Vec<u8> {
    let mut out = Vec::with_capacity(chain.len as usize);
    for &(t, s) in &chain.blocks {
        let block = image.sector(t, s).expect("followed");
        if block[0] == 0 {
            let last = usize::from(block[1].max(1));
            out.extend_from_slice(&block[2..=last]);
        } else {
            out.extend_from_slice(&block[2..]);
        }
    }
    out
}

/// The `n` sectors from `start` onward in disk order (track by track),
/// as a 1581 partition holds them; `None` if they run off the disk.
pub(crate) fn run(layout: Layout, start: (u8, u8), n: u32) -> Option<Vec<(u8, u8)>> {
    let first = layout.index(start.0, start.1)?;
    (first..first + n as usize)
        .map(|i| layout.position(i))
        .collect()
}

// ---------------------------------------------------------------------
// The BAM

/// Where one track's free count and bitmap are.
struct BamEntry {
    count: (u8, u8, usize),
    bitmap: (u8, u8, usize),
    bytes: usize,
}

/// The BAM of one directory: the whole disk's on a 1541 or 1571, or a
/// 1581 directory's (the main one or a partition's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Bam {
    layout: Layout,
    /// 1581: the track holding the BAM.
    track: u8,
    extra: ExtraTracks,
    /// D71: the second side is in the BAM (byte 3 of 18/0 has bit 7 set).
    double: bool,
}

impl Bam {
    pub fn new(image: &DiskImage, dir: &Dir) -> Self {
        let layout = image.layout();
        let header = image.sector(layout.directory_track(), 0).expect("on disk");
        Bam {
            layout,
            track: dir.track,
            extra: if layout == Layout::D64_40 {
                detect_extra(header)
            } else {
                ExtraTracks::Uncovered
            },
            double: layout == Layout::D71 && header[3] & 0x80 != 0,
        }
    }

    pub fn extra(&self) -> ExtraTracks {
        self.extra
    }

    fn entry(&self, t: u8) -> Option<BamEntry> {
        let four = |at: usize| BamEntry {
            count: (18, 0, at),
            bitmap: (18, 0, at + 1),
            bytes: 3,
        };
        match self.layout {
            Layout::D81 => {
                let (sector, base) = if t <= 40 { (1, t - 1) } else { (2, t - 41) };
                let at = 0x10 + 6 * usize::from(base);
                (1..=80).contains(&t).then_some(BamEntry {
                    count: (self.track, sector, at),
                    bitmap: (self.track, sector, at + 1),
                    bytes: 5,
                })
            }
            _ if (1..=35).contains(&t) => Some(four(4 * usize::from(t))),
            Layout::D64_40 if t <= 40 => match self.extra {
                ExtraTracks::SpeedDos => Some(four(0xC0 + 4 * usize::from(t - 36))),
                ExtraTracks::DolphinDos => Some(four(0xAC + 4 * usize::from(t - 36))),
                ExtraTracks::Uncovered => None,
            },
            Layout::D71 if self.double && t <= 70 => Some(BamEntry {
                count: (18, 0, 0xDD + usize::from(t - 36)),
                bitmap: (53, 0, 3 * usize::from(t - 36)),
                bytes: 3,
            }),
            _ => None,
        }
    }

    /// Does the BAM cover track `t`?
    pub fn covers(&self, t: u8) -> bool {
        self.entry(t).is_some()
    }

    /// The free count recorded for track `t`.
    pub fn recorded(&self, image: &DiskImage, t: u8) -> Option<u8> {
        let e = self.entry(t)?;
        Some(image.sector(e.count.0, e.count.1).ok()?[e.count.2])
    }

    /// The bitmap of track `t` (bit `s` set = sector `s` free).
    fn bitmap(&self, image: &DiskImage, t: u8) -> Option<u64> {
        let e = self.entry(t)?;
        let block = image.sector(e.bitmap.0, e.bitmap.1).ok()?;
        let mut bits = 0u64;
        for i in 0..e.bytes {
            bits |= u64::from(block[e.bitmap.2 + i]) << (8 * i);
        }
        Some(bits)
    }

    /// Free sectors the bitmap marks on track `t`.
    pub fn counted(&self, image: &DiskImage, t: u8) -> Option<u8> {
        let n = u32::from(self.layout.sectors_in_track(t)?);
        let mask = (1u64 << n) - 1;
        Some((self.bitmap(image, t)? & mask).count_ones() as u8)
    }

    /// Is the sector free? `None` if the BAM doesn't cover its track.
    pub fn is_free(&self, image: &DiskImage, t: u8, s: u8) -> Option<bool> {
        Some(self.bitmap(image, t)? >> s & 1 == 1)
    }

    /// Mark a sector free or used, keeping the track's count in step.
    pub fn set(&self, image: &mut DiskImage, t: u8, s: u8, free: bool) -> Result<()> {
        let Some(e) = self.entry(t) else {
            return Err(Error::Damaged(format!(
                "track {t} is outside the disk's block availability map"
            )));
        };
        let was = self.is_free(image, t, s) == Some(true);
        if was == free {
            return Ok(());
        }
        let block = image.sector_mut(e.bitmap.0, e.bitmap.1)?;
        let byte = e.bitmap.2 + usize::from(s / 8);
        block[byte] ^= 1 << (s % 8);
        let block = image.sector_mut(e.count.0, e.count.1)?;
        let count = &mut block[e.count.2];
        *count = if free {
            count.wrapping_add(1)
        } else {
            count.wrapping_sub(1)
        };
        Ok(())
    }

    /// Can files be given blocks on track `t` of `dir`? Not on the
    /// directory track, nor on track 53 of a 1571 disk (its second BAM).
    pub fn for_files(&self, dir: &Dir, t: u8) -> bool {
        (dir.lo..=dir.hi).contains(&t)
            && t != dir.track
            && !(self.layout == Layout::D71 && t == 53)
            && self.covers(t)
    }

    /// Blocks free, as the drive's directory listing counts them: the
    /// recorded counts of every track files can use.
    pub fn blocks_free(&self, image: &DiskImage, dir: &Dir) -> u32 {
        (dir.lo..=dir.hi)
            .filter(|&t| self.for_files(dir, t))
            .map(|t| u32::from(self.recorded(image, t).unwrap_or(0)))
            .sum()
    }
}

/// How a 40-track disk keeps tracks 36–40 in its BAM: the four bytes of
/// each track must make sense (the count matching the bitmap) and not all
/// be zero.
fn detect_extra(header: &[u8]) -> ExtraTracks {
    let fits = |at: usize| {
        let mut any = false;
        for i in 0..5 {
            let e = &header[at + 4 * i..at + 4 * i + 4];
            let bits = u32::from(e[1]) | u32::from(e[2]) << 8 | u32::from(e[3]) << 16;
            if bits >> 17 != 0 || bits.count_ones() != u32::from(e[0]) {
                return false;
            }
            any |= e[0] != 0;
        }
        any
    };
    if fits(0xC0) {
        ExtraTracks::SpeedDos
    } else if fits(0xAC) {
        ExtraTracks::DolphinDos
    } else {
        ExtraTracks::Uncovered
    }
}

// ---------------------------------------------------------------------
// Choosing free blocks, as the drives do

/// Hands out free blocks the way the drive's DOS does. A block is free
/// only if the BAM says so *and* nothing on the disk uses it.
pub(crate) struct Allocator<'a> {
    pub bam: Bam,
    pub dir: &'a Dir,
    /// Sectors something on the disk uses, by image index.
    pub taken: &'a mut Vec<bool>,
}

impl Allocator<'_> {
    fn usable(&self, image: &DiskImage, t: u8, s: u8) -> bool {
        let layout = image.layout();
        self.bam.for_files(self.dir, t)
            && self.bam.is_free(image, t, s) == Some(true)
            && layout.index(t, s).is_some_and(|n| !self.taken[n])
    }

    /// Blocks files could be given now.
    pub fn available(&self, image: &DiskImage) -> u32 {
        let layout = image.layout();
        (self.dir.lo..=self.dir.hi)
            .flat_map(|t| (0..layout.sectors_in_track(t).unwrap_or(0)).map(move |s| (t, s)))
            .filter(|&(t, s)| self.usable(image, t, s))
            .count() as u32
    }

    fn take(&mut self, image: &mut DiskImage, t: u8, s: u8) -> Result<(u8, u8)> {
        self.bam.set(image, t, s, false)?;
        let n = image.layout().index(t, s).expect("usable");
        self.taken[n] = true;
        Ok((t, s))
    }

    /// The first block of a file: the lowest free sector on the track
    /// nearest the directory track, looking below it first.
    pub fn first(&mut self, image: &mut DiskImage) -> Result<Option<(u8, u8)>> {
        let dir = self.dir.track;
        for d in 1..=image.layout().tracks() {
            let below = dir.checked_sub(d).filter(|&t| t >= self.dir.lo);
            let above = dir.checked_add(d).filter(|&t| t <= self.dir.hi);
            for t in [below, above].into_iter().flatten() {
                if let Some(s) = self.lowest(image, t, 0) {
                    return self.take(image, t, s).map(Some);
                }
            }
        }
        Ok(None)
    }

    /// The block after `(t, s)`: the interleave further on along the same
    /// track; when the track is full, the next track outward from the
    /// directory, and when that side is used up, the other side starting
    /// next to the directory track (counting the interleave from sector 0
    /// there, as the drives do).
    pub fn next(&mut self, image: &mut DiskImage, t: u8, s: u8) -> Result<Option<(u8, u8)>> {
        let layout = image.layout();
        let dir = self.dir.track;
        // Each track to try, with the sector the interleave counts from:
        // the last one on this side, sector 0 once it crosses over.
        let tracks: Vec<(u8, u8)> = if t < dir {
            let this_side = (self.dir.lo..=t).rev().map(|t2| (t2, s));
            this_side
                .chain((dir + 1..=self.dir.hi).map(|t2| (t2, 0)))
                .collect()
        } else {
            let this_side = (t..=self.dir.hi).map(|t2| (t2, s));
            this_side
                .chain((self.dir.lo..dir).rev().map(|t2| (t2, 0)))
                .collect()
        };
        for (track, base) in tracks {
            let count = layout.sectors_in_track(track).unwrap_or(0);
            if count == 0 {
                continue;
            }
            let from = step(base, layout.interleave(), count);
            if let Some(found) = self.lowest(image, track, from) {
                return self.take(image, track, found).map(Some);
            }
        }
        Ok(None)
    }

    /// The first usable sector on track `t` from sector `from` up,
    /// wrapping round to 0.
    fn lowest(&self, image: &DiskImage, t: u8, from: u8) -> Option<u8> {
        let count = image.layout().sectors_in_track(t)?;
        (0..count)
            .map(|i| (from + i) % count)
            .find(|&s| self.usable(image, t, s))
    }
}

/// The sector `interleave` on from `s` on a track of `count` sectors,
/// as the drives work it out: past the end, it wraps round to one less
/// than the overflow (so a track's sectors are visited in turn).
fn step(s: u8, interleave: u8, count: u8) -> u8 {
    let mut from = u16::from(s) + u16::from(interleave);
    let count = u16::from(count);
    while from >= count {
        from = (from - count).saturating_sub(1);
    }
    from as u8
}

/// How far apart the drive puts directory sectors: 3 on the 1541 and
/// 1571, 1 on the 1581.
fn dir_interleave(layout: Layout) -> u8 {
    if layout == Layout::D81 { 1 } else { 3 }
}

/// Add a sector to the end of a directory, on its own track, as the drive
/// does when every slot is used. Its slots are all empty.
pub(crate) fn grow_dir(
    image: &mut DiskImage,
    bam: &Bam,
    dir: &Dir,
    chain: &DirChain,
    taken: &mut [bool],
) -> Result<(u8, u8)> {
    let layout = image.layout();
    let &(lt, ls) = chain.sectors.last().expect("a directory has a sector");
    if lt != dir.track {
        return Err(Error::DirectoryFull);
    }
    let count = layout.sectors_in_track(dir.track).expect("on disk");
    let from = step(ls, dir_interleave(layout), count);
    let free = (0..count).map(|i| (from + i) % count).find(|&s| {
        bam.is_free(image, dir.track, s) == Some(true)
            && !taken[layout.index(dir.track, s).expect("on disk")]
    });
    let Some(s) = free else {
        return Err(Error::DirectoryFull);
    };
    bam.set(image, dir.track, s, false)?;
    taken[layout.index(dir.track, s).expect("on disk")] = true;
    let mut block = [0u8; SECTOR_SIZE];
    block[1] = 0xFF;
    image.write_sector(dir.track, s, &block)?;
    let last = image.sector_mut(lt, ls)?;
    last[0] = dir.track;
    last[1] = s;
    Ok((dir.track, s))
}

/// Write `data` as a chain of new blocks; returns the first block and
/// the number of blocks. An empty file gets one block holding a carriage
/// return, as the drives write one.
pub(crate) fn write_chain(
    image: &mut DiskImage,
    alloc: &mut Allocator<'_>,
    data: &[u8],
) -> Result<((u8, u8), u16)> {
    let data = if data.is_empty() { &[0x0D][..] } else { data };
    let needed = data.len().div_ceil(BLOCK_DATA);
    let free = alloc.available(image);
    if needed as u32 > free {
        return Err(Error::DiskFull {
            needed: needed as u32,
            free,
        });
    }
    if needed > usize::from(u16::MAX) {
        return Err(Error::Unsupported("files of 65536 blocks or more".into()));
    }
    let mut blocks = Vec::with_capacity(needed);
    let mut at = alloc.first(image)?.expect("counted");
    blocks.push(at);
    for _ in 1..needed {
        at = alloc.next(image, at.0, at.1)?.expect("counted");
        blocks.push(at);
    }
    for (i, piece) in data.chunks(BLOCK_DATA).enumerate() {
        let mut block = [0u8; SECTOR_SIZE];
        if let Some(&(t, s)) = blocks.get(i + 1) {
            block[0] = t;
            block[1] = s;
        } else {
            block[1] = (piece.len() + 1) as u8;
        }
        block[2..2 + piece.len()].copy_from_slice(piece);
        let (t, s) = blocks[i];
        image.write_sector(t, s, &block)?;
    }
    Ok((blocks[0], needed as u16))
}

// ---------------------------------------------------------------------
// Formatting

/// Format a whole disk, as the drive's `N0:name,id` (or c1541's
/// `format`) does: every other sector zero.
pub(crate) fn format(layout: Layout, name: &[u8], id: [u8; 2]) -> Result<DiskImage> {
    let mut image = DiskImage::new(layout);
    match layout {
        Layout::D64 | Layout::D71 => {
            let mut bam = [0u8; SECTOR_SIZE];
            bam[0] = 18;
            bam[1] = 1;
            bam[2] = b'A';
            bam[3] = if layout == Layout::D71 { 0x80 } else { 0 };
            bam[0x90..0xA0].copy_from_slice(&padded(name));
            bam[0xA0..0xAB].copy_from_slice(&[
                0xA0, 0xA0, id[0], id[1], 0xA0, b'2', b'A', 0xA0, 0xA0, 0xA0, 0xA0,
            ]);
            image.write_sector(18, 0, &bam)?;
            let mut dir = [0u8; SECTOR_SIZE];
            dir[1] = 0xFF;
            image.write_sector(18, 1, &dir)?;
            let root = Dir::root(layout);
            let bam = Bam::new(&image, &root);
            for t in 1..=layout.tracks() {
                if t == 53 {
                    continue; // the 1571 keeps all of track 53 for itself
                }
                for s in 0..layout.sectors_in_track(t).expect("on disk") {
                    if !(t == 18 && s <= 1) {
                        bam.set(&mut image, t, s, true)?;
                    }
                }
            }
        }
        Layout::D81 => {
            let root = Dir::root(layout);
            format_1581(&mut image, &root, name, id)?;
        }
        Layout::D64_40 => {
            return Err(Error::Unsupported(
                "formatting 40-track disks (not yet)".into(),
            ));
        }
    }
    Ok(image)
}

/// Write a 1581 header, BAM and empty directory for `dir` (the whole disk
/// or a partition), as the 1581 does: only `dir`'s tracks are free, less
/// sectors 0-3 of its directory track.
pub(crate) fn format_1581(
    image: &mut DiskImage,
    dir: &Dir,
    name: &[u8],
    id: [u8; 2],
) -> Result<()> {
    let t = dir.track;
    let mut header = [0u8; SECTOR_SIZE];
    header[0] = t;
    header[1] = 3;
    header[2] = b'D';
    header[4..0x14].copy_from_slice(&padded(name));
    header[0x14..0x1D].copy_from_slice(&[0xA0, 0xA0, id[0], id[1], 0xA0, b'3', b'D', 0xA0, 0xA0]);
    image.write_sector(t, 0, &header)?;
    for (s, link) in [(1u8, (t, 2u8)), (2, (0, 0xFF))] {
        let mut bam = [0u8; SECTOR_SIZE];
        bam[0] = link.0;
        bam[1] = link.1;
        bam[2] = b'D';
        bam[3] = 0xBB;
        bam[4] = id[0];
        bam[5] = id[1];
        bam[6] = 0xC0;
        image.write_sector(t, s, &bam)?;
    }
    let mut first = [0u8; SECTOR_SIZE];
    first[1] = 0xFF;
    image.write_sector(t, 3, &first)?;
    let bam = Bam::new(image, dir);
    for track in dir.lo..=dir.hi {
        for s in 0..40 {
            if !(track == t && s <= 3) {
                bam.set(image, track, s, true)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_types() {
        for code in 0..8 {
            assert_eq!(FileType::from_code(code).code(), code);
        }
        assert_eq!(FileType::from_code(0x82), FileType::Prg);
        assert_eq!(FileType::Prg.letters(), "PRG");
        assert!(FileType::Seq.writable() && !FileType::Rel.writable());
    }

    #[test]
    fn names_are_padded_and_trimmed() {
        assert_eq!(&padded(b"AB")[..4], &[b'A', b'B', 0xA0, 0xA0]);
        assert_eq!(trim_name(&padded(b"AB")), b"AB");
        assert_eq!(trim_name(&[b'A', 0xA0, b'B', 0xA0]), [b'A', 0xA0, b'B']);
        assert_eq!(trim_name(&[0xA0; 16]), b"");
    }

    #[test]
    fn extra_track_bam_styles() {
        let mut h = [0u8; 256];
        assert_eq!(detect_extra(&h), ExtraTracks::Uncovered);
        for i in 0..5 {
            h[0xC0 + 4 * i..0xC4 + 4 * i].copy_from_slice(&[17, 0xFF, 0xFF, 0x01]);
        }
        assert_eq!(detect_extra(&h), ExtraTracks::SpeedDos);
        h[0xC0] = 16;
        assert_eq!(detect_extra(&h), ExtraTracks::Uncovered);
        for i in 0..5 {
            h[0xAC + 4 * i..0xB0 + 4 * i].copy_from_slice(&[1, 0, 0, 0x01]);
        }
        assert_eq!(detect_extra(&h), ExtraTracks::DolphinDos);
    }
}
