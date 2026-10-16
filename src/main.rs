use std::path::{Path, PathBuf};
use std::process::exit;

use clap::Parser;
use unicode_width::UnicodeWidthStr;

use uniprint::{clear_cache, entries, init, render, uniprintln};

#[derive(Parser, Debug)]
#[command(
    name = "uniprint",
    version,
    about = "Grapheme-aware terminal output",
    long_about = None,
)]
struct Cli {
    /// Print every exception in the dictionary: uncorrected | corrected.
    #[arg(long)]
    dump: bool,

    /// Discard the current cache and rebuild from scratch.
    #[arg(long)]
    force: bool,

    /// Load an external test file (one string per line; '#' comments).
    #[arg(long, value_name = "FILE")]
    file: Option<PathBuf>,

    /// Interactively review exceptions and hand-edit the emit string.
    #[arg(long)]
    human: bool,
}

// ----------------------------------------------------------------- render

/// Turn raw ESC bytes into a visible notation so they survive being printed.
/// `\x1b[1C`  ->  `\e[1C`
fn visualize(s: &str) -> String {
    s.replace('\x1b', "\\e").replace('\x08', "\\b")
}

const CORRECTION_COL: u16 = 12;
const METRICS_COL:    u16 = 36;

fn print_table(rows: &[(String, uniprint::Entry)]) {
    if rows.is_empty() {
        println!("(no exceptions)");
        return;
    }
    const CORRECTION_COL: u16 = 12;
    const METRICS_COL:    u16 = 36;

    for (input, entry) in rows {
        print!(
            "{}\x1b[{}G({})",
            input,
            CORRECTION_COL,
            visualize(&entry.emit),
        );
        print!(
            "\x1b[{}GE:{:>2} M:{:>2}   in E:{:>2} M:{:>2}",
            METRICS_COL,
            entry.expected, entry.measured,
            entry.input_expected, entry.input_measured,
        );
        println!();
    }
}

fn process_file(path: &Path) -> std::io::Result<Vec<(String, uniprint::Entry)>> {
    let content = std::fs::read_to_string(path)?;
    let map: std::collections::HashMap<String, uniprint::Entry> =
        uniprint::entries().into_iter().collect();

    let mut rows = Vec::new();
    for (lineno, raw) in content.lines().enumerate() {
        let line = raw.trim_end_matches('\r');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.chars().any(|c| c == '\t') {
            eprintln!("warning: line {}: tab in input, ignoring", lineno + 1);
            continue;
        }
        let entry = map.get(line).cloned().unwrap_or_else(|| {
            let w = line.width() as i32;
            uniprint::Entry {
                emit: line.to_string(),
                expected: w,
                measured: w,
                input_expected: w,
                input_measured: w,
                suggested: line.to_string(),
                human_reviewed: false,
            }
        });
        rows.push((line.to_string(), entry));
    }
    Ok(rows)
}



// -------------------------------------------------------------------- main
   
fn main() {
    let cli = Cli::parse();

    
    if cli.force {
        if let Err(e) = clear_cache() {
            eprintln!("uniprint: clear cache: {}", e);
            exit(1);
        }
        eprintln!("uniprint: cache cleared, rebuilding on init");
    }

    if let Err(e) = init() {
        eprintln!("uniprint: init: {}", e);
        exit(1);
    }

    if cli.human {
        if let Err(e) = uniprint::human_review() {
            eprintln!("uniprint: human review: {}", e);
            exit(1);
        }
        return;
    }

    if cli.dump {
        let rows = entries();
        eprintln!("uniprint: {} exception(s)", rows.len());
        print_table(&rows);
        return;
    }

    if let Some(path) = cli.file.as_deref() {
        match process_file(path) {
            Ok(rows) => print_table(&rows),
            Err(e) => {
                eprintln!("uniprint: {}: {}", path.display(), e);
                exit(1);
            }
        }
        return;
    }

    // No flags: demo output.
    uniprintln!("Hello, world.");
    uniprintln!("Dancer:      🕺.");
    uniprintln!("Golfer:      🏌.");
    uniprintln!("Lady golfer: 🏌🏼‍♀️.");
    uniprintln!("Family:      👨‍👩‍👧‍👦.");
    uniprintln!("Flag:        🇨🇦.");
    uniprintln!("Keycap:      1️⃣.");
    uniprintln!("Thumbs:      👍🏽.");
}