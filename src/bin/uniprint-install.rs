//! uniprint-install — move calibration profiles between the project's
//! `./assets/profiles` directory and the user's per-terminal profile store.
//!
//! The store location is computed by `uniprint::terminals_dir()` so the two
//! binaries can never drift apart.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::exit;

use clap::Parser;
use uniprint::terminals_dir;

/// Default source/destination for profile exchange. Relative to CWD so the
/// tool is run from the project root.
const ASSETS_DIR: &str = "assets/profiles";

#[derive(Parser, Debug)]
#[command(
    name = "uniprint-install",
    version,
    about = "Install (or retrieve) uniprint terminal profiles",
    long_about = None,
)]
struct Cli {
    /// Copy profiles from the user library back into ./assets/profiles.
    #[arg(long)]
    retrieve: bool,

    /// Overwrite existing files instead of skipping them.
    #[arg(long)]
    force: bool,
}

fn assets_dir() -> PathBuf {
    PathBuf::from(ASSETS_DIR)
}

/// Copy every `*.json` from `src` into `dst`. Returns (copied, skipped, failed).
fn copy_profiles(src: &Path, dst: &Path, force: bool) -> io::Result<(usize, usize, usize)> {
    if !src.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("source directory does not exist: {}", src.display()),
        ));
    }
    fs::create_dir_all(dst)?;

    let mut copied = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();

        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }

        let Some(name) = path.file_name() else {
            failed += 1;
            continue;
        };
        let target = dst.join(name);

        if target.exists() && !force {
            skipped += 1;
            continue;
        }

        match fs::copy(&path, &target) {
            Ok(_) => copied += 1,
            Err(e) => {
                eprintln!("uniprint-install: copy {}: {}", path.display(), e);
                failed += 1;
            }
        }
    }

    Ok((copied, skipped, failed))
}

fn main() {
    let cli = Cli::parse();

    let lib_dir = terminals_dir();
    let assets  = assets_dir();

    let (from, to, verb) = if cli.retrieve {
        (&lib_dir, &assets, "Retrieving")
    } else {
        (&assets, &lib_dir, "Installing")
    };

    println!("{} profiles", verb);
    println!("  from {}", from.display());
    println!("  to   {}", to.display());

    match copy_profiles(from, to, cli.force) {
        Ok((copied, skipped, failed)) => {
            println!(
                "{} copied, {} skipped, {} failed",
                copied, skipped, failed
            );
            if failed > 0 {
                exit(1);
            }
        }
        Err(e) => {
            eprintln!("uniprint-install: {}", e);
            exit(1);
        }
    }
}