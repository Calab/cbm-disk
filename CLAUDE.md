# cbm-disk: notes for Claude

A standalone Rust library for Commodore disk images (D64, D71, D81) and
their CBM DOS file system, built the same way as atari-disk
(`~/Coding/atari-disk`): same design, new code. Started 2026-11-25 for
Steve's Commodore editor. Licence MIT OR Apache-2.0. Repo:
github.com/Calab/cbm-disk. The agreed plan and test findings are in
`tmp/PLAN.md`.

## Working with Steve
- Steve Norton is the author, new to Rust: explain cargo steps plainly.
  Uses VS Code on Ubuntu.
- Commit only when asked; end messages with the Co-Authored-By line. Never
  push. `gh` is not installed.
- Files for Steve to review go in `tmp/` (git-ignored).
- Discuss and agree a plan before starting work on anything new.
- CI must pass `cargo fmt --check`, `cargo clippy --all-targets -- -D
  warnings` and `cargo test`. No dependencies.
- `#![warn(missing_docs)]`; key calls have runnable examples; disk-format
  facts go in `src/formats.rs`.

## Design (agreed with Steve)
- Copy atari-disk's approach wherever possible; if it has a shortcoming,
  note it in `tmp/PLAN.md` and update atari-disk too (Steve, 2026-11-25).
- Mirrors atari-disk's API (`Volume`, `DiskImage`, `DirEntry`, `Features`,
  `validate`, `sector_map`, ...). No file I/O; every change all or nothing.
- **Mimic the real Commodore drives as closely as possible.** The real
  drive ROMs in x64sc are the reference; c1541 second.
- Paths use `/` (1581 partitions). Names: anything the drive can store;
  new names 1-16 bytes, not ending in `$A0`.
- REL: list and read only. GEOS: list (details reported), read the plain
  chain; VLIR later, don't design it out. D81 partitions: full support.
- Changes refused only if the directory chain is broken; new blocks must
  be free in the BAM and used by nothing.

## Samples and testing
- Samples from Steve's NAS: `/mnt/NAS_Storage/Retro Computing &
  Emulation/Commodore/` (C64/! Software/Disks, C128/! Software). Copied
  into `samples/D64|D71|D81` (git-ignored, copyrighted).
- VICE 3.10: `c1541`, `petcat`, `x64sc`. Drive ROMs in
  `/usr/local/share/vice/DRIVES`. Unattended runs: put a tokenized BASIC
  program on the image, then
  `x64sc -default -warp +sound -drive8type 1581 -drive8truedrive -8 X.d81
  -autostart X.d81 -limitcycles N -exitscreenshot shot.png`.
