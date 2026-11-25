//! [`DiskImage`]: the sectors of a D64, D71 or D81 file, and the
//! [`Layout`]s it can have.

use crate::error::{Error, Result};

/// The bytes in every sector (a "block") of these disks.
pub const SECTOR_SIZE: usize = 256;

/// The kinds of disk image, each a copy of one Commodore drive's disks.
/// The size of an image file tells them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Layout {
    /// `.D64`, a 1541 disk: 35 tracks, 683 sectors, 664 blocks free when
    /// formatted.
    D64,
    /// `.D64` with 40 tracks (768 sectors), as some 1541 speeders
    /// (SpeedDOS, DolphinDOS) and copiers used. Tracks 36–40 are only
    /// written to if the disk keeps their free blocks in one of those
    /// DOSes' ways (see [`crate::Volume::extra_tracks`]).
    #[allow(non_camel_case_types)]
    D64_40,
    /// `.D71`, a 1571 disk: both sides of a 1541 disk, 70 tracks, 1366
    /// sectors, 1328 blocks free when formatted.
    D71,
    /// `.D81`, a 1581 disk: 80 tracks of 40 sectors (3200), 3160 blocks
    /// free when formatted.
    D81,
}

impl Layout {
    /// Every layout, smallest first.
    pub const ALL: [Layout; 4] = [Layout::D64, Layout::D64_40, Layout::D71, Layout::D81];

    /// The number of tracks. Tracks are numbered from 1.
    pub fn tracks(self) -> u8 {
        match self {
            Layout::D64 => 35,
            Layout::D64_40 => 40,
            Layout::D71 => 70,
            Layout::D81 => 80,
        }
    }

    /// The sectors on `track` (numbered from 0), or `None` if there's no
    /// such track. 1541 disks have fewer sectors on the inner tracks: 21
    /// on tracks 1–17, 19 on 18–24, 18 on 25–30 and 17 from 31.
    pub fn sectors_in_track(self, track: u8) -> Option<u8> {
        if track == 0 || track > self.tracks() {
            return None;
        }
        Some(match self {
            Layout::D81 => 40,
            Layout::D71 if track > 35 => zone(track - 35),
            _ => zone(track),
        })
    }

    /// All the sectors on the disk.
    pub fn sector_count(self) -> u32 {
        (1..=self.tracks())
            .map(|t| u32::from(self.sectors_in_track(t).unwrap_or(0)))
            .sum()
    }

    /// The length of an image file without error bytes.
    pub fn image_len(self) -> usize {
        self.sector_count() as usize * SECTOR_SIZE
    }

    /// The track holding the directory: 18 on 1541 and 1571 disks, 40 on
    /// 1581 disks.
    pub fn directory_track(self) -> u8 {
        match self {
            Layout::D81 => 40,
            _ => 18,
        }
    }

    /// The drive these disks are for: `"1541"`, `"1571"` or `"1581"`.
    pub fn drive(self) -> &'static str {
        match self {
            Layout::D64 | Layout::D64_40 => "1541",
            Layout::D71 => "1571",
            Layout::D81 => "1581",
        }
    }

    /// The file name extension, without the dot: `"d64"`, `"d71"` or
    /// `"d81"`.
    pub fn extension(self) -> &'static str {
        match self {
            Layout::D64 | Layout::D64_40 => "d64",
            Layout::D71 => "d71",
            Layout::D81 => "d81",
        }
    }

    /// How far apart the drive puts a file's blocks on a track: 10 on the
    /// 1541, 6 on the 1571 and 1 on the 1581.
    pub fn interleave(self) -> u8 {
        match self {
            Layout::D64 | Layout::D64_40 => 10,
            Layout::D71 => 6,
            Layout::D81 => 1,
        }
    }

    /// Where `(track, sector)` is in the image: the sector's index, from
    /// 0 for track 1 sector 0. `None` if the disk has no such sector.
    pub fn index(self, track: u8, sector: u8) -> Option<usize> {
        let count = self.sectors_in_track(track)?;
        if sector >= count {
            return None;
        }
        let before: u32 = (1..track)
            .map(|t| u32::from(self.sectors_in_track(t).unwrap_or(0)))
            .sum();
        Some(before as usize + usize::from(sector))
    }

    /// The `(track, sector)` at index `n` (see [`Layout::index`]).
    pub fn position(self, n: usize) -> Option<(u8, u8)> {
        let mut left = n;
        for t in 1..=self.tracks() {
            let count = usize::from(self.sectors_in_track(t)?);
            if left < count {
                return Some((t, left as u8));
            }
            left -= count;
        }
        None
    }

    /// Every `(track, sector)` on the disk, in image order.
    pub fn sectors(self) -> impl Iterator<Item = (u8, u8)> {
        (1..=self.tracks())
            .flat_map(move |t| (0..self.sectors_in_track(t).unwrap_or(0)).map(move |s| (t, s)))
    }

    /// The layout an image file of `len` bytes holds, and whether it ends
    /// with error bytes.
    fn from_len(len: usize) -> Option<(Layout, bool)> {
        Self::ALL.into_iter().find_map(|l| {
            let plain = l.image_len();
            if len == plain {
                Some((l, false))
            } else if len == plain + l.sector_count() as usize {
                Some((l, true))
            } else {
                None
            }
        })
    }
}

/// Sectors on 1541 track `t` (1-40).
fn zone(t: u8) -> u8 {
    match t {
        1..=17 => 21,
        18..=24 => 19,
        25..=30 => 18,
        _ => 17,
    }
}

/// A Commodore disk image in memory: its sectors, and the error bytes some
/// images carry (one per sector, recording read errors on the original
/// disk, often copy protection). Sectors are named as CBM DOS names them:
/// tracks from 1, sectors from 0.
///
/// ```
/// use cbm_disk::{DiskImage, Layout};
///
/// let mut image = DiskImage::new(Layout::D64);
/// image.write_sector(1, 0, &[0x55; 256])?;
/// assert_eq!(image.sector(1, 0)?[0], 0x55);
/// assert_eq!(image.to_bytes().len(), 174_848);
/// # Ok::<(), cbm_disk::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskImage {
    layout: Layout,
    data: Vec<u8>,
    errors: Option<Vec<u8>>,
}

impl DiskImage {
    /// A disk of `layout` with every byte zero and no error bytes:
    /// unformatted, as far as the drive is concerned (see
    /// [`crate::Volume::format`]).
    pub fn new(layout: Layout) -> Self {
        Self {
            layout,
            data: vec![0; layout.image_len()],
            errors: None,
        }
    }

    /// Read an image file's bytes. The length tells the layout:
    ///
    /// | Layout | Bytes | With error bytes |
    /// |---|---|---|
    /// | D64 | 174,848 | 175,531 |
    /// | D64, 40 tracks | 196,608 | 197,376 |
    /// | D71 | 349,696 | 351,062 |
    /// | D81 | 819,200 | 822,400 |
    ///
    /// Any other length fails with [`Error::NotAnImage`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let (layout, has_errors) = Layout::from_len(bytes.len()).ok_or_else(|| {
            Error::NotAnImage(format!(
                "{} bytes is not the size of a D64, D71 or D81 image",
                bytes.len()
            ))
        })?;
        let (data, errors) = bytes.split_at(layout.image_len());
        Ok(Self {
            layout,
            data: data.to_vec(),
            errors: has_errors.then(|| errors.to_vec()),
        })
    }

    /// The image file's bytes, error bytes and all, ready to save.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = self.data.clone();
        if let Some(errors) = &self.errors {
            out.extend_from_slice(errors);
        }
        out
    }

    /// Which kind of disk this is.
    pub fn layout(&self) -> Layout {
        self.layout
    }

    /// Sector `sector` of track `track`: 256 bytes.
    pub fn sector(&self, track: u8, sector: u8) -> Result<&[u8]> {
        let at = self.offset(track, sector)?;
        Ok(&self.data[at..at + SECTOR_SIZE])
    }

    /// The sector to change in place.
    pub fn sector_mut(&mut self, track: u8, sector: u8) -> Result<&mut [u8]> {
        let at = self.offset(track, sector)?;
        Ok(&mut self.data[at..at + SECTOR_SIZE])
    }

    /// Replace a sector: `bytes` must be exactly 256 long.
    pub fn write_sector(&mut self, track: u8, sector: u8, bytes: &[u8]) -> Result<()> {
        if bytes.len() != SECTOR_SIZE {
            return Err(Error::WrongSectorLength { got: bytes.len() });
        }
        self.sector_mut(track, sector)?.copy_from_slice(bytes);
        Ok(())
    }

    /// Does the image end with error bytes?
    pub fn has_error_bytes(&self) -> bool {
        self.errors.is_some()
    }

    /// The error byte of a sector, if the image has them. `$01` (and `$00`)
    /// mean the sector read without error; others are the drive's error
    /// codes, e.g. `$05` for a checksum error (DOS error 23).
    pub fn error_byte(&self, track: u8, sector: u8) -> Option<u8> {
        let n = self.layout.index(track, sector)?;
        self.errors.as_ref().map(|e| e[n])
    }

    /// Add error bytes (all `$01`, "no error") if the image has none, or
    /// with `false` drop them.
    pub fn set_error_bytes(&mut self, on: bool) {
        match (on, &self.errors) {
            (true, None) => self.errors = Some(vec![1; self.layout.sector_count() as usize]),
            (false, Some(_)) => self.errors = None,
            _ => {}
        }
    }

    /// Set one sector's error byte (see [`DiskImage::error_byte`]),
    /// adding error bytes to the image first if it has none.
    pub fn set_error_byte(&mut self, track: u8, sector: u8, code: u8) -> Result<()> {
        let n = self
            .layout
            .index(track, sector)
            .ok_or(Error::SectorOutOfRange { track, sector })?;
        self.set_error_bytes(true);
        if let Some(errors) = &mut self.errors {
            errors[n] = code;
        }
        Ok(())
    }

    fn offset(&self, track: u8, sector: u8) -> Result<usize> {
        self.layout
            .index(track, sector)
            .map(|n| n * SECTOR_SIZE)
            .ok_or(Error::SectorOutOfRange { track, sector })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry() {
        assert_eq!(Layout::D64.sector_count(), 683);
        assert_eq!(Layout::D64_40.sector_count(), 768);
        assert_eq!(Layout::D71.sector_count(), 1366);
        assert_eq!(Layout::D81.sector_count(), 3200);
        assert_eq!(Layout::D64.index(18, 0), Some(357));
        assert_eq!(Layout::D71.index(53, 0), Some(683 + 357));
        assert_eq!(Layout::D81.index(40, 0), Some(1560));
        assert_eq!(Layout::D64.index(18, 19), None);
        assert_eq!(Layout::D64.index(36, 0), None);
        assert_eq!(Layout::D64.index(0, 0), None);
        for l in Layout::ALL {
            for (n, (t, s)) in l.sectors().enumerate() {
                assert_eq!(l.index(t, s), Some(n));
                assert_eq!(l.position(n), Some((t, s)));
            }
            assert_eq!(l.position(l.sector_count() as usize), None);
        }
    }

    #[test]
    fn sizes_and_error_bytes() {
        for (len, layout, errors) in [
            (174_848, Layout::D64, false),
            (175_531, Layout::D64, true),
            (196_608, Layout::D64_40, false),
            (197_376, Layout::D64_40, true),
            (349_696, Layout::D71, false),
            (351_062, Layout::D71, true),
            (819_200, Layout::D81, false),
            (822_400, Layout::D81, true),
        ] {
            let mut bytes = vec![0; len];
            bytes[len - 1] = 0x05;
            let image = DiskImage::from_bytes(&bytes).unwrap();
            assert_eq!(image.layout(), layout);
            assert_eq!(image.has_error_bytes(), errors);
            assert_eq!(image.to_bytes(), bytes);
        }
        assert!(matches!(
            DiskImage::from_bytes(&[0; 1000]),
            Err(Error::NotAnImage(_))
        ));

        let mut image = DiskImage::new(Layout::D64);
        assert_eq!(image.error_byte(1, 0), None);
        image.set_error_byte(35, 16, 5).unwrap();
        assert_eq!(image.error_byte(35, 16), Some(5));
        assert_eq!(image.error_byte(1, 0), Some(1));
        assert_eq!(image.to_bytes().len(), 175_531);
        image.set_error_bytes(false);
        assert_eq!(image.to_bytes().len(), 174_848);
    }

    #[test]
    fn sectors() {
        let mut image = DiskImage::new(Layout::D81);
        image.write_sector(80, 39, &[7; 256]).unwrap();
        assert_eq!(image.to_bytes()[819_200 - 1], 7);
        assert!(matches!(
            image.write_sector(1, 0, &[0; 255]),
            Err(Error::WrongSectorLength { got: 255 })
        ));
        assert!(matches!(
            image.sector(81, 0),
            Err(Error::SectorOutOfRange {
                track: 81,
                sector: 0
            })
        ));
    }
}
