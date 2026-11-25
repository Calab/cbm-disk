//! List everything on a Commodore disk image, then check it.
//!
//! ```text
//! cargo run --example ls -- GAMES.D64
//! ```
//!
//! Shows the disk as the drive's directory listing would (`LOAD"$",8`),
//! with every 1581 subdirectory listed in turn, then any problems
//! `validate` finds.

use cbm_disk::{DiskImage, Volume, petscii::petscii_to_ascii};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: cargo run --example ls -- DISK.D64");
        std::process::exit(2);
    };
    let image = DiskImage::from_bytes(&std::fs::read(&path)?)?;
    let vol = Volume::open(image)?;
    println!(
        "{path}: {:?} ({} drive){}{}",
        vol.layout(),
        vol.layout().drive(),
        if vol.image().has_error_bytes() {
            ", with error bytes"
        } else {
            ""
        },
        if vol.is_geos() { ", GEOS disk" } else { "" },
    );
    println!();
    println!(
        "0 \"{:<16}\" {} {}",
        petscii_to_ascii(&vol.disk_name()),
        petscii_to_ascii(&vol.disk_id()),
        petscii_to_ascii(&vol.dos_type())
    );
    list(&vol, b"", 0)?;
    println!("{} blocks free.", vol.blocks_free());

    let problems = vol.validate();
    println!();
    if problems.is_empty() {
        println!("No problems found.");
    }
    for problem in problems {
        println!("Problem: {problem}");
    }
    Ok(())
}

/// Print directory `dir` and, indented, the directories inside it.
fn list(vol: &Volume, dir: &[u8], depth: usize) -> cbm_disk::Result<()> {
    for entry in vol.list(dir)? {
        let indent = "  ".repeat(depth);
        let mut notes = String::new();
        if entry.damaged {
            notes.push_str(" DAMAGED");
        }
        if entry.geos.is_some() {
            notes.push_str(" GEOS");
        }
        println!(
            "{indent}{:<5}\"{}\"{:pad$} {}{notes}",
            entry.blocks,
            entry.name_text(),
            "",
            entry.type_text(),
            pad = 16 - entry.name.len().min(16),
        );
        if entry.is_dir() {
            // Paths join names with `/`, as the stored PETSCII bytes.
            let inner = if dir.is_empty() {
                entry.name.clone()
            } else {
                [dir, b"/", &entry.name].concat()
            };
            list(vol, &inner, depth + 1)?;
            println!("{indent}  {} blocks free.", vol.dir_blocks_free(&inner)?);
        }
    }
    Ok(())
}
