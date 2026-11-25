//! How Commodore disk images and CBM DOS are laid out, byte by byte, and
//! how the drives choose blocks. This page has no code; it's the
//! reference the crate is built from. Where sources disagree, what the
//! real drive ROMs do (run in VICE with true drive emulation) wins.
//!
//! # The image files
//!
//! A `.D64`, `.D71` or `.D81` file is the disk's sectors one after
//! another, track 1 sector 0 first, with no header. The file's length is
//! the only clue to which it is. Some images add one *error byte* per
//! sector at the end, in the same order: `$01` (or `$00`) for a sector
//! that read cleanly, otherwise the drive's error code for it (e.g. `$05`
//! for a checksum error, DOS error 23), usually copy protection.
//!
//! | Image | Drive | Tracks | Sectors | Bytes | With error bytes |
//! |---|---|---|---|---|---|
//! | D64 | 1541 | 35 | 683 | 174,848 | 175,531 |
//! | D64 | 1541 + speeder | 40 | 768 | 196,608 | 197,376 |
//! | D71 | 1571 | 70 | 1366 | 349,696 | 351,062 |
//! | D81 | 1581 | 80 | 3200 | 819,200 | 822,400 |
//!
//! Every sector ("block") holds 256 bytes. Tracks count from 1, sectors
//! from 0. On the 1541 the outer tracks hold more sectors:
//!
//! | Tracks | Sectors each |
//! |---|---|
//! | 1–17 | 21 (0–20) |
//! | 18–24 | 19 |
//! | 25–30 | 18 |
//! | 31–40 | 17 |
//!
//! A D71 is two of these: tracks 36–70 are the second side, laid out as
//! tracks 1–35. A D81 has 40 sectors on every track.
//!
//! # Files: chains of blocks
//!
//! A file is a chain of blocks. Bytes 0–1 of each block are the track and
//! sector of the next; the other 254 bytes are data. In the last block
//! byte 0 is `$00` and byte 1 is the index of the last data byte (so `$01`
//! means none, `$FF` a full block). The drives leave whatever was in their
//! buffer after that; this crate writes zeros. An empty file is one block
//! holding a single `$0D` (carriage return): that's what the real drives
//! save.
//!
//! # 1541 (D64): header and BAM, track 18 sector 0
//!
//! | Bytes | Holds |
//! |---|---|
//! | `$00`–`$01` | 18, 1: the first directory sector |
//! | `$02` | DOS version, `$41` (`A`). Any other value: the 1541 won't write ("soft" write protection) |
//! | `$03` | `$00` (D71: `$80` = double-sided) |
//! | `$04`–`$8F` | BAM: 4 bytes a track for tracks 1–35 |
//! | `$90`–`$9F` | disk name, padded with `$A0` |
//! | `$A0`–`$A1` | `$A0` |
//! | `$A2`–`$A3` | disk ID |
//! | `$A4` | `$A0` |
//! | `$A5`–`$A6` | DOS type, `2A` |
//! | `$A7`–`$AA` | `$A0` |
//! | `$AB`–`$FF` | `$00` (but see GEOS and 40 tracks below) |
//!
//! Each track's 4 BAM bytes are its free count, then a bitmap: bit 0 of
//! the first byte is sector 0, a set bit means free. A new disk has all
//! of track 18 free except sectors 0 and 1, so 664 blocks free. The
//! directory listing's "blocks free" adds up the counts of every track
//! except 18.
//!
//! ## 40-track disks
//!
//! Tracks 36–40 have no place in the 1541's BAM. Speeders that used them
//! kept 4 more bytes a track: SpeedDOS at `$C0`–`$D3`, DolphinDOS at
//! `$AC`–`$BF`. Most 40-track images (including every sample tested) have
//! neither, so this crate treats those tracks as outside the BAM and never
//! writes there unless one of those layouts is found (each track's count
//! matching its bitmap, and not all zero).
//!
//! # 1571 (D71)
//!
//! 18/0 as on the 1541, with byte `$03` = `$80`. The second side's free
//! counts are at `$DD`–`$FF` of 18/0 (one byte a track, 36–70) and its
//! bitmaps in 53/0 (3 bytes a track from `$00`). The 1571 keeps all of
//! track 53 for itself: formatted with count 0 and every bit clear, so
//! 1328 blocks free. If byte `$03` hasn't bit 7 set, the disk is
//! single-sided as far as the drive is concerned.
//!
//! # 1581 (D81)
//!
//! | Sector | Holds |
//! |---|---|
//! | 40/0 | header: `$00`–`$01` = 40, 3; `$02` = `D`; `$04`–`$13` disk name; `$14`–`$15` `$A0`; `$16`–`$17` ID; `$18` `$A0`; `$19`–`$1A` `3D`; `$1B`–`$1C` `$A0`; the rest zero |
//! | 40/1 | BAM for tracks 1–40: `$00`–`$01` = 40, 2; `$02` = `D`; `$03` = `$BB`; `$04`–`$05` ID; `$06` = `$C0`; `$07` auto-boot flag; from `$10` 6 bytes a track (count, then a 5-byte bitmap) |
//! | 40/2 | BAM for tracks 41–80, the same, with `$00`–`$01` = `$00`, `$FF` |
//! | 40/3 on | the directory |
//!
//! A new disk has track 40 free except sectors 0–3: 3160 blocks free.
//!
//! ## Partitions and subdirectories
//!
//! The 1581's `/0:NAME,` command (with track, sector and size in blocks)
//! turns a run of free sectors into a partition: an entry of type `CBM`
//! (`$85`) whose start is the first sector and whose block count is its
//! size; its sectors are marked used in the BAM. A partition that starts
//! at sector 0, is a whole number of tracks (at least 3, so 120 blocks)
//! and doesn't include the directory's own track can be *formatted* as a
//! subdirectory: `/0:NAME` selects it, then `N0:NAME,ID` writes on its
//! first track a header (sector 0), BAM (sectors 1–2, as 40/0–40/2) and
//! directory (from sector 3). Checked with the real 1581 ROM:
//!
//! * In the partition's BAM, tracks outside it have count 0 and every bit
//!   clear; its first track has sectors 0–3 used.
//! * Its files go on its other tracks, never its first (even when every
//!   other block is used, the drive reports 72 DISK FULL).
//! * Its "blocks free" is its tracks less the first: 4 tracks give 120.
//! * Partitions can nest: a subdirectory can hold partitions of its own.
//!
//! Paths in this crate name a subdirectory with `/` as the 1581 does:
//! `GAMES/PACMAN`.
//!
//! # Directory entries
//!
//! A directory sector holds 8 entries of 32 bytes; bytes 0–1 of the
//! sector (the first entry's first 2 bytes) link to the next directory
//! sector. A 1541 or 1571 directory holds 144 entries (18 sectors of
//! track 18), a 1581's 296 (37 sectors of track 40).
//!
//! | Bytes | Holds |
//! |---|---|
//! | `$02` | type: bits 0–2 the kind (0 DEL, 1 SEQ, 2 PRG, 3 USR, 4 REL, 5 CBM), bit 6 locked (`<`), bit 7 closed (clear: a "splat" file, `*PRG`); `$00` is an empty or deleted entry |
//! | `$03`–`$04` | first block |
//! | `$05`–`$14` | name, padded with `$A0` |
//! | `$15`–`$16` | REL: first side sector; GEOS: info block |
//! | `$17` | REL: record length; GEOS: 0 sequential, 1 VLIR |
//! | `$18` | GEOS file type |
//! | `$19`–`$1D` | GEOS: date (year − 1900, month, day, hour, minute) |
//! | `$1E`–`$1F` | blocks, as the listing shows (not checked by DOS) |
//!
//! A deleted ("scratched") file keeps its entry with type `$00`; a new
//! file takes the first such slot. Names are any bytes the drive can
//! store; a name can hold `$A0`, but the padding makes trailing ones
//! vanish. Directory art uses all of this.
//!
//! ## REL files
//!
//! `$15`–`$16` lead to the side sectors, chained like a file. Each holds
//! (from `$10`) the track and sector of up to 120 data blocks; the data is
//! an ordinary chain. On a 1581 the chain starts with a "super side
//! sector". This crate reads the data and owns all these blocks, but
//! doesn't write REL files.
//!
//! ## GEOS
//!
//! A GEOS disk has `GEOS format V1.x` at `$AD` of the header sector (18/0
//! or 40/0), and at `$AB`–`$AC` the "border" block: one more directory
//! sector, for files dragged to the desktop's border, which DOS doesn't
//! list. Each GEOS file has an info block (`$15`–`$16`: icon, description,
//! ...). A VLIR file's first block is an index of up to 127 records (track
//! and sector pairs from `$02`; 0/0 ends it, 0/`$FF` is an empty record),
//! each an ordinary chain.
//!
//! # Telling file systems apart
//!
//! [`crate::Volume::detect`] looks only at the header and the first
//! directory sector, so a CBM disk damaged elsewhere still counts as CBM
//! DOS. The DOS version and DOS type bytes aren't used: copy protection and
//! directory art change them (`$42`, `92`, ...), and the drives ignore them
//! when reading.
//!
//! * **C128 CP/M** (1541 and 1571 disks): track 1 sector 0 starts with
//!   `CBM` (the C128 boot sector, which the CP/M BIOS checks for; its last
//!   byte is `$FF` on a double-sided disk), *and* the header links to 1/5
//!   instead of 18/1. A `CBM` boot sector alone is any C128 boot disk.
//! * **1581 CP/M**: the CBM directory holds only a USR file called
//!   `COPYRIGHT CBM 86`, and no blocks are free; CP/M's own directory is at
//!   1/0.
//! * **C64 CP/M** disks are ordinary CBM disks: one PRG called `CPM`, with
//!   every other block marked used. They count as CBM DOS.
//! * **CBM DOS**: the header links to the first directory sector (18/1, or
//!   40/3 on a 1581), or that sector looks like one (its link is `00 FF`
//!   or another sector on the directory track). The drives always start
//!   the directory there, whatever the header says.
//! * Anything else (an unformatted disk, say) is raw: sectors only.
//!
//! # How the drives choose blocks
//!
//! The crate copies this exactly; it was checked block for block against
//! the real 1541, 1571 and 1581 ROMs and VICE's `c1541`.
//!
//! * **The first block** of a file: the lowest free sector on the track
//!   nearest the directory track, trying the track below it first (17, 19,
//!   16, 20, ... on a 1541).
//! * **The next block**: the last sector plus the *interleave* (10 on the
//!   1541, 6 on the 1571, 1 on the 1581). Past the end of the track it
//!   wraps round to one less than the overflow (sector 20 + 10 on a
//!   21-sector track gives 9); if that sector is used, the next one up,
//!   round the track.
//! * **A full track**: the next track outward from the directory track,
//!   counting the interleave from the last sector (on that track's
//!   sector count). When that side of the directory is used up, the
//!   other side, starting next to the directory track and counting the
//!   interleave from sector 0. On a D71: 17 down to 1, then 19 up to 70,
//!   skipping 53.
//! * **The directory** grows on its own track only, 3 sectors at a time
//!   on the 1541 and 1571 (18/1, 18/4, 18/7, ..., wrapping the same way),
//!   1 at a time on the 1581.
//! * Data never goes on the directory track (or track 53 of a 1571).
//!
//! The 1541's `@:` save-and-replace writes the new file before freeing the
//! old one (and has a famous bug); this crate frees the old blocks first.
