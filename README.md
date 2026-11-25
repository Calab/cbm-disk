# cbm-disk

A Rust library for Commodore disk images: `.D64` (1541, 35 or 40
tracks), `.D71` (1571) and `.D81` (1581, with partitions as
subdirectories), and the CBM DOS files on them. No dependencies.

It behaves like the real drives: new files get their blocks the way the
1541, 1571 and 1581 choose them, and disks it writes have been checked
in VICE with the real drive ROMs (block for block against what the
drives write, and the drives' own validate command).

**Status:** new; not yet on crates.io, and the API may still change.

## Using it

Add it to your `Cargo.toml`:

```toml
[dependencies]
cbm-disk = { git = "https://github.com/Calab/cbm-disk" }
```

Then:

```rust
use cbm_disk::{DiskImage, FileType, Volume};

let bytes = std::fs::read("GAMES.D64")?;
let mut vol = Volume::open(DiskImage::from_bytes(&bytes)?)?;

for entry in vol.list("")? {            // "" is the main directory
    println!("{:<5}\"{}\" {}", entry.blocks, entry.name_text(), entry.type_text());
}
let game = vol.read("PACMAN")?;          // a PRG's load address included
vol.write("PACMAN COPY", FileType::Prg, &game)?;

std::fs::write("GAMES.D64", vol.image().to_bytes())?;
```

The library works on the image in memory and does no file I/O of its
own: your program reads the file, and saves `vol.image().to_bytes()`
the way it likes. Every change either succeeds completely or leaves the
disk as it was.

Names are PETSCII bytes, exactly as stored, so every file can be
reached, directory art included. Capitals, digits and most punctuation
are the same in ASCII, so `"PACMAN"` works as it is; `cbm_disk::petscii`
converts the rest.

## What it does

- Read D64, 40-track D64, D71 and D81 images, with or without error
  bytes (kept and written back as they were).
- List, read, write (PRG, SEQ, USR), rename, delete, lock and unlock
  files; set the disk name and ID; format blank disks.
- 1581 subdirectories: list, read and write inside them, make and delete
  them, move files between them. Paths use `/` as the 1581 does.
- REL files and GEOS files are listed and read (GEOS VLIR files: the
  record index only, for now); they aren't written.
- `validate()` checks a disk without changing it; `sector_map()` says
  what every block holds.
- Full sector access (`read_sector`, `write_sector`) on any disk. A disk
  with no CBM DOS on it opens as raw, and C128 CP/M disks are recognised
  as CP/M: both give sector access only.

Not yet: writing REL or GEOS files, formatting 40-track disks, D80/D82,
and G64 (raw GCR) images.

## Running the examples and tests

```sh
cargo run --example ls -- GAMES.D64   # list a disk and check it
cargo test                            # the tests
cargo doc --open                      # the documentation
```

Real disk images in `samples/D64`, `samples/D71` and `samples/D81` are
tested too when present (they aren't in the repository).

## Licence

MIT or Apache-2.0, at your option.
