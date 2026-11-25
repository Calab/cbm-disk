//! Real disk images from `samples/D64`, `samples/D71` and `samples/D81`
//! (not in git; each missing folder is skipped). Every image must read and
//! write back byte for byte, every file that isn't damaged must read, and
//! a change must keep the rest of the disk as it was.

use std::path::{Path, PathBuf};

use cbm_disk::{DirEntry, DiskImage, Error, FileType, Layout, Problem, Volume};

fn images() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in ["D64", "D71", "D81"] {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("samples")
            .join(dir);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            eprintln!("skipping {} (not found)", dir.display());
            continue;
        };
        out.extend(entries.flatten().map(|e| e.path()).filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| ["d64", "d71", "d81"].contains(&e.to_ascii_lowercase().as_str()))
        }));
    }
    out.sort();
    out
}

fn open(path: &Path) -> (Vec<u8>, Volume) {
    let bytes = std::fs::read(path).unwrap();
    let image = DiskImage::from_bytes(&bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    (bytes, Volume::open(image).unwrap())
}

/// Every entry in a directory and the directories inside it, with paths.
fn walk(vol: &Volume, dir: &[u8], out: &mut Vec<(Vec<u8>, DirEntry)>) {
    for e in vol.list(dir).unwrap() {
        let path = if dir.is_empty() {
            e.name.clone()
        } else {
            [dir, b"/", &e.name].concat()
        };
        if e.is_dir() {
            walk(vol, &path, out);
        }
        out.push((path, e));
    }
}

#[test]
fn every_sample_round_trips() {
    for path in images() {
        let (bytes, vol) = open(&path);
        println!(
            "{}: {:?}{}, {} blocks free",
            path.file_name().unwrap().to_string_lossy(),
            vol.layout(),
            if vol.image().has_error_bytes() {
                " with error bytes"
            } else {
                ""
            },
            vol.blocks_free()
        );
        assert!(
            vol.image().to_bytes() == bytes,
            "{} changed",
            path.display()
        );
    }
}

/// Every file reads, unless it's marked damaged (then reading says so),
/// and its length matches the listing.
#[test]
fn every_file_reads() {
    for path in images() {
        let (_, vol) = open(&path);
        let mut entries = Vec::new();
        walk(&vol, b"", &mut entries);
        for (p, e) in entries {
            if e.is_dir() {
                assert!(matches!(vol.read(&p), Err(Error::IsDirectory(_))));
                continue;
            }
            let geos_vlir = e.geos.is_some_and(|g| g.vlir);
            match vol.read(&p) {
                Ok(data) if !geos_vlir && e.file_type != FileType::Cbm => {
                    assert_eq!(
                        data.len() as u32,
                        e.size,
                        "{}: {}",
                        path.display(),
                        e.name_text()
                    );
                }
                Ok(_) => {}
                Err(Error::Damaged(_)) => assert!(e.damaged && e.closed, "{}", e.name_text()),
                Err(other) => panic!("{}: {}: {other}", path.display(), e.name_text()),
            }
        }
    }
}

/// Writing a file to a copy of each sample that's sound changes only what
/// the write should, and leaves it sound.
#[test]
fn writing_to_sound_samples_keeps_them_sound() {
    for path in images() {
        let (_, mut vol) = open(&path);
        if !vol.validate().is_empty() || vol.blocks_free() < 3 {
            continue;
        }
        let before = vol.list("").unwrap();
        match vol.write("CBM-DISK TEST", FileType::Seq, &[0x41; 600]) {
            Ok(()) => {}
            Err(Error::DirectoryFull | Error::DiskFull { .. }) => continue,
            Err(e) => panic!("{}: {e}", path.display()),
        }
        let problems: Vec<Problem> = vol.validate();
        assert!(problems.is_empty(), "{}: {problems:?}", path.display());
        assert_eq!(vol.read("CBM-DISK TEST").unwrap(), [0x41; 600]);
        // The new file takes the first empty slot, as the drive does;
        // everything else is as it was.
        let after = vol.list("").unwrap();
        let others: Vec<_> = after
            .into_iter()
            .filter(|e| e.name != b"CBM-DISK TEST")
            .collect();
        assert_eq!(others, before, "{}", path.display());
        vol.delete("CBM-DISK TEST").unwrap();
        assert!(vol.validate().is_empty(), "{}", path.display());
    }
}

/// The layouts the samples have.
#[test]
fn sample_layouts() {
    for path in images() {
        let (bytes, vol) = open(&path);
        let layout = vol.layout();
        let extra = if vol.image().has_error_bytes() {
            layout.sector_count() as usize
        } else {
            0
        };
        assert_eq!(bytes.len(), layout.image_len() + extra);
        if layout == Layout::D64_40 {
            assert!(vol.extra_tracks().is_some());
        }
    }
}
