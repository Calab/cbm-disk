//! What every sector of a disk is used for, and the problems found on the
//! way: the work behind [`crate::Volume::sector_map`] and
//! [`crate::Volume::validate`]. Writes use it too, so a new file never
//! takes a block something else uses. Nothing here changes a disk.

use std::fmt;

use crate::cbmdos::{self, Bam, Dir, FileType, Raw};
use crate::image::{DiskImage, Layout};
use crate::petscii::petscii_to_ascii;

/// What a sector holds, from [`crate::Volume::sector_map`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SectorUse {
    /// Marked free, and nothing uses it.
    Free,
    /// The disk's own records: the header and BAM (18/0 on a 1541 disk,
    /// 18/0 and 53/0 on a 1571, 40/0–40/2 on a 1581, and the first three
    /// sectors of each 1581 subdirectory).
    System,
    /// Kept out of use by the drive: the rest of track 53 on a 1571
    /// disk.
    Reserved,
    /// Part of the directory at this path (PETSCII; empty for the main
    /// one).
    Directory(Vec<u8>),
    /// Part of the file at this path (PETSCII): its data, and for REL
    /// files their side sectors, for GEOS files their info block and VLIR
    /// index, and for a 1581 partition that isn't a subdirectory all of
    /// it.
    File(Vec<u8>),
    /// Marked in use, but nothing uses it (a "lost" block).
    Unowned,
    /// On a track the BAM doesn't cover (tracks 36–40 of most 40-track
    /// disks), and used by nothing.
    Outside,
}

/// A problem [`crate::Volume::validate`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Problem {
    /// `owner` uses the block, but the BAM says it's free, so the drive
    /// could give it to a new file and overwrite `owner`.
    MarkedFree {
        /// Its track.
        track: u8,
        /// Its sector.
        sector: u8,
        /// What uses it.
        owner: SectorUse,
    },
    /// Two files or directories claim the same block.
    Shared {
        /// Its track.
        track: u8,
        /// Its sector.
        sector: u8,
        /// The first to claim it.
        first: SectorUse,
        /// The second.
        second: SectorUse,
    },
    /// Marked in use, but nothing uses it: space lost until the disk is
    /// validated by the drive (`V0:`).
    Unowned {
        /// Its track.
        track: u8,
        /// Its sector.
        sector: u8,
    },
    /// A track's free count doesn't match its bitmap.
    FreeCount {
        /// The directory whose BAM it is (PETSCII; empty for the main
        /// one).
        dir: Vec<u8>,
        /// The track.
        track: u8,
        /// What the BAM's count says.
        recorded: u8,
        /// The free bits in the bitmap.
        counted: u8,
    },
    /// A file or directory (PETSCII path) can't be followed to its end.
    Damaged {
        /// Its path (PETSCII; empty for the main directory).
        path: Vec<u8>,
        /// What's wrong, in words.
        why: String,
    },
    /// A file that was never closed (a "splat" file, `*PRG` in a
    /// listing). The drive's validate command deletes these.
    Unclosed {
        /// Its path (PETSCII).
        path: Vec<u8>,
    },
}

fn path_text(p: &[u8], what: &str) -> String {
    if p.is_empty() {
        "the main directory".into()
    } else {
        format!("{what} {}", petscii_to_ascii(p))
    }
}

impl fmt::Display for SectorUse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SectorUse::Free => write!(f, "free"),
            SectorUse::System => write!(f, "the disk's header or BAM"),
            SectorUse::Reserved => write!(f, "reserved"),
            SectorUse::Directory(p) => write!(f, "{}", path_text(p, "directory")),
            SectorUse::File(p) => write!(f, "{}", path_text(p, "file")),
            SectorUse::Unowned => write!(f, "in use by nothing"),
            SectorUse::Outside => write!(f, "outside the BAM"),
        }
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Problem::MarkedFree {
                track,
                sector,
                owner,
            } => write!(
                f,
                "block {track}/{sector} is marked free but belongs to {owner}"
            ),
            Problem::Shared {
                track,
                sector,
                first,
                second,
            } => write!(
                f,
                "block {track}/{sector} belongs to both {first} and {second}"
            ),
            Problem::Unowned { track, sector } => {
                write!(
                    f,
                    "block {track}/{sector} is marked in use but nothing uses it"
                )
            }
            Problem::FreeCount {
                dir,
                track,
                recorded,
                counted,
            } => write!(
                f,
                "{}: track {track} says {recorded} blocks are free, but its bitmap marks {counted}",
                if dir.is_empty() {
                    "the BAM".to_string()
                } else {
                    format!("the BAM of {}", petscii_to_ascii(dir))
                }
            ),
            Problem::Damaged { path, why } => {
                write!(f, "{} is damaged: {why}", path_text(path, "file"))
            }
            Problem::Unclosed { path } => {
                write!(f, "file {} was never closed", petscii_to_ascii(path))
            }
        }
    }
}

/// The blocks an entry owns, and why it couldn't be followed if it
/// couldn't. A subdirectory's blocks are its whole partition.
pub(crate) fn entry_blocks(image: &DiskImage, raw: &Raw) -> (Vec<(u8, u8)>, Option<String>) {
    let layout = image.layout();
    let mut blocks = Vec::new();
    let mut broken = None;
    let mut chain_from = |start: (u8, u8), blocks: &mut Vec<(u8, u8)>| {
        let chain = cbmdos::follow(image, start);
        blocks.extend(chain.blocks);
        if broken.is_none() {
            broken = chain.broken;
        }
    };
    let start = raw.start();
    match raw.file_type() {
        FileType::Cbm => match cbmdos::run(layout, start, u32::from(raw.blocks())) {
            Some(run) => blocks = run,
            None => broken = Some("the partition runs off the disk".into()),
        },
        _ if start.0 == 0 => {} // no blocks at all (e.g. directory art)
        FileType::Rel => {
            chain_from(start, &mut blocks);
            if raw.side().0 != 0 {
                chain_from(raw.side(), &mut blocks);
            }
        }
        _ if raw.is_vlir(layout) => {
            chain_from(start, &mut blocks);
            if let Ok(index) = image.sector(start.0, start.1) {
                for pair in index[2..].chunks(2) {
                    match (pair[0], pair[1]) {
                        (0, 0) => break,
                        (0, _) => continue,
                        record => chain_from(record, &mut blocks),
                    }
                }
            }
            blocks.push(raw.side());
        }
        _ => {
            chain_from(start, &mut blocks);
            if raw.is_geos(layout) {
                blocks.push(raw.side());
            }
        }
    }
    (blocks, broken)
}

/// Everything found on the disk.
pub(crate) struct Scan {
    pub map: Vec<SectorUse>,
    pub problems: Vec<Problem>,
}

impl Scan {
    /// Is the sector used by anything (by image index)?
    pub fn taken(&self) -> Vec<bool> {
        self.map
            .iter()
            .map(|u| !matches!(u, SectorUse::Free | SectorUse::Unowned | SectorUse::Outside))
            .collect()
    }
}

struct Walker<'a> {
    image: &'a DiskImage,
    owner: Vec<Option<SectorUse>>,
    /// The directory each sector belongs to (index into `dirs`).
    area: Vec<usize>,
    dirs: Vec<Dir>,
    /// Sectors of each directory given over to partitions inside it.
    partitions: Vec<Vec<(u8, u8)>>,
    problems: Vec<Problem>,
}

impl Walker<'_> {
    fn claim(&mut self, (t, s): (u8, u8), what: &SectorUse) {
        let Some(n) = self.image.layout().index(t, s) else {
            return;
        };
        match &self.owner[n] {
            Some(first) => self.problems.push(Problem::Shared {
                track: t,
                sector: s,
                first: first.clone(),
                second: what.clone(),
            }),
            None => self.owner[n] = Some(what.clone()),
        }
    }

    fn walk(&mut self, dir: Dir, depth: usize) {
        let me = self.dirs.len();
        self.dirs.push(dir.clone());
        self.partitions.push(Vec::new());
        let layout = self.image.layout();
        if me > 0 {
            for t in dir.lo..=dir.hi {
                for s in 0..layout.sectors_in_track(t).unwrap_or(0) {
                    self.area[layout.index(t, s).expect("on disk")] = me;
                }
            }
            for s in 0..3 {
                self.claim((dir.track, s), &SectorUse::System);
            }
        }
        let chain = cbmdos::dir_chain(self.image, &dir);
        for &at in &chain.sectors {
            self.claim(at, &SectorUse::Directory(dir.path.clone()));
        }
        if let Some(why) = &chain.broken {
            self.problems.push(Problem::Damaged {
                path: dir.path.clone(),
                why: format!("its directory can't be followed: {why}"),
            });
        }
        let entries: Vec<Raw> = cbmdos::slots(self.image, &chain)
            .into_iter()
            .filter(|r| r.type_byte() != 0)
            .collect();
        for raw in entries {
            let path = dir.child(&raw.name());
            if raw.file_type() == FileType::Cbm {
                let (blocks, broken) = entry_blocks(self.image, &raw);
                if let Some(why) = broken {
                    self.problems.push(Problem::Damaged { path, why });
                    continue;
                }
                self.partitions[me].extend(&blocks);
                match cbmdos::subdir(self.image, &dir, &raw) {
                    Some(sub) if depth < 16 => self.walk(sub, depth + 1),
                    _ => {
                        for b in blocks {
                            self.claim(b, &SectorUse::File(path.clone()));
                        }
                    }
                }
                continue;
            }
            let (blocks, broken) = entry_blocks(self.image, &raw);
            for b in blocks {
                self.claim(b, &SectorUse::File(path.clone()));
            }
            if !raw.closed() {
                self.problems.push(Problem::Unclosed { path });
            } else if let Some(why) = broken {
                self.problems.push(Problem::Damaged { path, why });
            }
        }
    }
}

/// A GEOS disk's border block, if it has one.
pub(crate) fn geos_border(image: &DiskImage) -> Option<(u8, u8)> {
    let layout = image.layout();
    let header = image.sector(layout.directory_track(), 0).ok()?;
    if header[0xAD..0xB8] != *b"GEOS format" {
        return None;
    }
    let at = (header[0xAB], header[0xAC]);
    layout.index(at.0, at.1).map(|_| at)
}

/// Walk the whole disk: every directory, partition and file.
pub(crate) fn scan(image: &DiskImage) -> Scan {
    let layout = image.layout();
    let n = layout.sector_count() as usize;
    let mut w = Walker {
        image,
        owner: vec![None; n],
        area: vec![0; n],
        dirs: Vec::new(),
        partitions: Vec::new(),
        problems: Vec::new(),
    };
    let root = Dir::root(layout);
    match layout {
        Layout::D81 => (0..3).for_each(|s| w.claim((40, s), &SectorUse::System)),
        Layout::D71 => {
            w.claim((18, 0), &SectorUse::System);
            w.claim((53, 0), &SectorUse::System);
            for s in 1..layout.sectors_in_track(53).expect("on disk") {
                w.claim((53, s), &SectorUse::Reserved);
            }
        }
        _ => w.claim((18, 0), &SectorUse::System),
    }
    w.walk(root.clone(), 0);
    // A GEOS disk keeps one more directory sector, the "border" block
    // (files dragged to the desktop's border), pointed to from $AB of the
    // header. DOS doesn't list it, but its files are real.
    if let Some(border) = geos_border(image) {
        w.claim(border, &SectorUse::Directory(Vec::new()));
        let chain = cbmdos::DirChain {
            sectors: vec![border],
            broken: None,
        };
        for raw in cbmdos::slots(image, &chain) {
            if raw.type_byte() == 0 {
                continue;
            }
            let path = root.child(&raw.name());
            let (blocks, broken) = entry_blocks(image, &raw);
            for b in blocks {
                w.claim(b, &SectorUse::File(path.clone()));
            }
            if let Some(why) = broken {
                w.problems.push(Problem::Damaged { path, why });
            }
        }
    }

    // Each directory's BAM against what's really used in its area.
    let mut map = vec![SectorUse::Free; n];
    for (d, dir) in w.dirs.iter().enumerate() {
        let bam = Bam::new(image, dir);
        let parts: std::collections::HashSet<(u8, u8)> = w.partitions[d].iter().copied().collect();
        for t in dir.lo..=dir.hi {
            if let (Some(recorded), Some(counted)) = (bam.recorded(image, t), bam.counted(image, t))
                && recorded != counted
            {
                w.problems.push(Problem::FreeCount {
                    dir: dir.path.clone(),
                    track: t,
                    recorded,
                    counted,
                });
            }
            for s in 0..layout.sectors_in_track(t).unwrap_or(0) {
                let i = layout.index(t, s).expect("on disk");
                let free = bam.is_free(image, t, s);
                if parts.contains(&(t, s)) {
                    // A partition inside this directory: its blocks must
                    // be marked used here, whatever they hold.
                    if free == Some(true) {
                        let owner = w.owner[i].clone().unwrap_or(SectorUse::Directory(
                            w.dirs
                                .get(w.area[i])
                                .map(|d| d.path.clone())
                                .unwrap_or_default(),
                        ));
                        w.problems.push(Problem::MarkedFree {
                            track: t,
                            sector: s,
                            owner,
                        });
                    }
                    continue;
                }
                if w.area[i] != d {
                    continue;
                }
                map[i] = match (&w.owner[i], free) {
                    (Some(owner), Some(true)) => {
                        w.problems.push(Problem::MarkedFree {
                            track: t,
                            sector: s,
                            owner: owner.clone(),
                        });
                        owner.clone()
                    }
                    (Some(owner), _) => owner.clone(),
                    (None, Some(true)) => SectorUse::Free,
                    (None, Some(false)) => {
                        w.problems.push(Problem::Unowned {
                            track: t,
                            sector: s,
                        });
                        SectorUse::Unowned
                    }
                    (None, None) => SectorUse::Outside,
                };
            }
        }
    }
    // Owners outside every directory's tracks (e.g. a chain running onto
    // a track the BAM doesn't cover) still show.
    for (i, owner) in w.owner.iter().enumerate() {
        if let Some(owner) = owner
            && matches!(map[i], SectorUse::Free | SectorUse::Outside)
        {
            map[i] = owner.clone();
        }
    }
    Scan {
        map,
        problems: w.problems,
    }
}
