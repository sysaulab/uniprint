//! uniprint — grapheme-aware terminal output with automatic width correction.
//!
//! Pipeline:  segment -> sanitize(caps) -> lookup(width) -> emit + correct.
//!
//! Capabilities (probed once at init, cached per terminal fingerprint) tell the
//! sanitizer which grapheme classes the terminal over-advances on. Classes that
//! over-advance are structurally replaced with a shorter fallback; classes that
//! under-advance are kept and padded with spaces at emit time.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const FORMAT_VERSION: u32 = 6;
const PROBE_TIMEOUT_MS: u64 = 250;
const PROBE_ROW: u16 = 2;

//Cascade corrections.

const PAD_THRESHOLD: i32 = 1;

// ================================================================ fingerprint


/// Coarse terminal identity used for profile sharing.
///
/// Everything environment-specific (LANG, LC_CTYPE, TMUX, session ids, term
/// program hashes…) is deliberately dropped. The fingerprint is
/// `{terminal}-{version}-{os}`, so two users running the same terminal build
/// on the same OS land on the same profile.
fn terminal_name() -> String {
    if let Ok(v) = std::env::var("TERM_PROGRAM") {
        if !v.is_empty() {
            return v;
        }
    }
    if std::env::var_os("KITTY_WINDOW_ID").is_some()   { return "kitty".into(); }
    if std::env::var_os("WEZTERM_VERSION").is_some()   { return "WezTerm".into(); }
    if std::env::var_os("GHOSTTY_VERSION").is_some()   { return "ghostty".into(); }
    if std::env::var_os("WT_SESSION").is_some()        { return "WindowsTerminal".into(); }
    if std::env::var_os("ALACRITTY_SOCKET").is_some()  { return "Alacritty".into(); }
    if std::env::var_os("VTE_VERSION").is_some()       { return "VTE".into(); }
    if let Ok(t) = std::env::var("TERM") {
        if !t.is_empty() {
            return t;
        }
    }
    "unknown".into()
}

fn terminal_version() -> String {
    for v in [
        "TERM_PROGRAM_VERSION",
        "WEZTERM_VERSION",
        "GHOSTTY_VERSION",
        "VTE_VERSION",
    ] {
        if let Ok(val) = std::env::var(v) {
            if !val.is_empty() {
                return val;
            }
        }
    }
    "unknown".into()
}

fn os_name() -> String {
    std::env::consts::OS.to_string()
}

fn fingerprint() -> String {
    format!("{}-{}-{}", terminal_name(), terminal_version(), os_name())
}

/// Convert a fingerprint into a filesystem-safe file name.
/// Bytes outside `[A-Za-z0-9._-]` become `_`.
fn profile_filename(fp: &str) -> String {
    let mut out = String::with_capacity(fp.len() + 5);
    for c in fp.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    out.push_str(".json");
    out
}


fn strip_vs16(s: &str) -> String {
    s.chars().filter(|c| *c != '\u{FE0F}').collect()
}

fn strip_vs16_and_skin(s: &str) -> String {
    s.chars()
        .filter(|c| {
            *c != '\u{FE0F}' && !(0x1F3FB..=0x1F3FF).contains(&(*c as u32))
        })
        .collect()
}

/// Ordered sequence of progressively simpler forms of `g`. Stage 0 is the
/// input itself. Each subsequent stage is strictly simpler. Deduplicated.
fn cascade_stages(g: &str, _caps: &Capabilities) -> Vec<String> {
    let mut stages: Vec<String> = Vec::with_capacity(5);
    let mut push = |s: String, v: &mut Vec<String>| {
        if !v.contains(&s) {
            v.push(s);
        }
    };

    push(g.to_string(), &mut stages);

    // 1. VS16 removed.
    if g.contains('\u{FE0F}') {
        push(strip_vs16(g), &mut stages);
    }

    // 2. VS16 + skin tone removed.
    let has_skin = g.chars().any(|c| (0x1F3FB..=0x1F3FF).contains(&(c as u32)));
    if has_skin {
        push(strip_vs16_and_skin(g), &mut stages);
    }

    // 3. Truncate at first ZWJ, then strip VS16 and skin tone.
    if let Some(idx) = g.find('\u{200D}') {
        push(strip_vs16_and_skin(&g[..idx]), &mut stages);
    }

    // 4. First surviving codepoint only (the base pictograph).
    let first = g.chars().find(|c| {
        !(0x1F3FB..=0x1F3FF).contains(&(*c as u32))
            && *c != '\u{FE0F}'
            && *c != '\u{200D}'
    });
    if let Some(c) = first {
        push(c.to_string(), &mut stages);
    }

    stages
}


// ===================================================================== types

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Deviation {
    TooShort,
    Correct,
    TooLong,
}

impl Deviation {
    fn from_diff(diff: i32) -> Self {
        match diff.cmp(&0) {
            std::cmp::Ordering::Less => Deviation::TooShort,
            std::cmp::Ordering::Equal => Deviation::Correct,
            std::cmp::Ordering::Greater => Deviation::TooLong,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Capabilities {
    pub cpr: bool,
    pub zwj: Deviation,
    pub regional: Deviation,
    pub keycap: Deviation,
    pub vs16: Deviation,
    pub skin_tone: Deviation,
}

impl Capabilities {
    pub fn disabled() -> Self {
        Self {
            cpr: false,
            zwj: Deviation::Correct,
            regional: Deviation::Correct,
            keycap: Deviation::Correct,
            vs16: Deviation::Correct,
            skin_tone: Deviation::Correct,
        }
    }
}


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub emit: String,
    pub expected: i32,
    pub measured: i32,
    pub input_expected: i32,
    pub input_measured: i32,
    #[serde(default)]
    pub suggested: String,
    #[serde(default)]
    pub human_reviewed: bool,
}


// ================================================================ fingerprint

fn fnv1a_64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn fingerprint_hash(fp: &str) -> String {
    format!("{:016x}", fnv1a_64(fp.as_bytes()))
}

// ===================================================================== cache


#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheFile {
    format_version: u32,
    fingerprint: String,
    generated_at: String,
    capabilities: Capabilities,
    entries: HashMap<String, Entry>,
}

fn cache_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "uniprint")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Directory holding per-terminal profile JSON files.
///
/// Public so `uniprint-install` writes to exactly the same location the
/// runtime reads from. Never duplicate this path logic elsewhere.
pub fn terminals_dir() -> PathBuf {
    cache_dir().join("terminals")
}

fn now_iso8601() -> String {
    // Cheap wall-clock stamp; no chrono dependency.
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("unix:{}", d.as_secs())
}

// ======================================================================= tty

struct Tty {
    file: File,
    orig: libc::termios,
    raw: bool,
    alt: bool,
}

impl Tty {
    fn open() -> io::Result<Self> {
        let file = File::options().read(true).write(true).open("/dev/tty")?;
        let fd = file.as_raw_fd();
        let mut t = unsafe { std::mem::zeroed::<libc::termios>() };
        if unsafe { libc::tcgetattr(fd, &mut t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { file, orig: t, raw: false, alt: false })
    }

    fn write(&mut self, s: &str) -> io::Result<()> {
        self.file.write_all(s.as_bytes())?;
        self.file.flush()
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }

    fn enter_alt(&mut self) -> io::Result<()> {
        if self.alt { return Ok(()); }
        self.write("\x1b[?1049h\x1b[?7l\x1b[2J\x1b[H")?;
        self.alt = true;
        Ok(())
    }

    fn leave_alt(&mut self) -> io::Result<()> {
        if !self.alt { return Ok(()); }
        self.write("\x1b[?7h\x1b[?1049l")?;
        self.alt = false;
        Ok(())
    }

    fn enter_raw(&mut self) -> io::Result<()> {
        if self.raw { return Ok(()); }
        let fd = self.file.as_raw_fd();
        let mut t = self.orig;
        t.c_iflag &= !(libc::ICRNL | libc::INLCR | libc::IGNCR | libc::IXON);
        t.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
        t.c_cc[libc::VMIN] = 0;
        t.c_cc[libc::VTIME] = 1;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        self.raw = true;
        Ok(())
    }

    fn leave_raw(&mut self) -> io::Result<()> {
        if !self.raw { return Ok(()); }
        let fd = self.file.as_raw_fd();
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &self.orig) } != 0 {
            return Err(io::Error::last_os_error());
        }
        self.raw = false;
        Ok(())
    }

    fn probe(&mut self, grapheme: &str) -> io::Result<Option<u16>> {
        self.write(&format!(
            "\x1b[{};1H\x1b[2K{}\x1b[6n",
            PROBE_ROW, grapheme
        ))?;

        let mut buf: Vec<u8> = Vec::with_capacity(64);
        let mut byte = [0u8; 1];
        let deadline = Instant::now() + Duration::from_millis(PROBE_TIMEOUT_MS);

        while Instant::now() < deadline {
            match self.file.read(&mut byte) {
                Ok(0) => continue,
                Ok(_) => {
                    buf.push(byte[0]);
                    if byte[0] == b'R' {
                        if let Some(col) = parse_cpr(&buf) {
                            return Ok(Some(col));
                        }
                        buf.clear();
                    }
                    if buf.len() > 128 {
                        buf.clear();
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }
}

impl Drop for Tty {
    fn drop(&mut self) {
        // Order matters: leave alt screen first (so the cursor lands back on
        // the primary screen's saved row/col), then restore termios.
        let _ = self.leave_alt();
        let _ = self.leave_raw();
    }
}

fn parse_cpr(buf: &[u8]) -> Option<u16> {
    let mut i = 0;
    while i + 2 < buf.len() {
        if buf[i] == 0x1b && buf[i + 1] == b'[' {
            let mut j = i + 2;
            let mut row: u32 = 0;
            let mut saw_row = false;
            while j < buf.len() && buf[j].is_ascii_digit() {
                row = row.saturating_mul(10) + (buf[j] - b'0') as u32;
                j += 1;
                saw_row = true;
            }
            if saw_row && j < buf.len() && buf[j] == b';' {
                j += 1;
                let mut col: u32 = 0;
                let mut saw_col = false;
                while j < buf.len() && buf[j].is_ascii_digit() {
                    col = col.saturating_mul(10) + (buf[j] - b'0') as u32;
                    j += 1;
                    saw_col = true;
                }
                if saw_col && j < buf.len() && buf[j] == b'R' {
                    return Some(col as u16);
                }
            }
        }
        i += 1;
    }
    None
}

// ================================================================= sanitize

fn is_regional(c: char) -> bool {
    (0x1F1E6..=0x1F1FF).contains(&(c as u32))
}

fn ri_pair(s: &str) -> Option<(char, char)> {
    let mut it = s.chars();
    let a = it.next()?;
    let b = it.next()?;
    if it.next().is_some() || !is_regional(a) || !is_regional(b) {
        return None;
    }
    Some((a, b))
}

fn ri_to_ascii(a: char, b: char) -> String {
    let ca = (b'A' + (a as u32 - 0x1F1E6) as u8) as char;
    let cb = (b'A' + (b as u32 - 0x1F1E6) as u8) as char;
    format!("{}{}", ca, cb)
}

pub fn sanitize(g: &str, caps: &Capabilities) -> String {
    let mut s = g.to_string();

    // ZWJ sequences: truncate at the first joiner.
    if caps.zwj == Deviation::TooLong {
        if let Some(i) = s.find('\u{200D}') {
            s.truncate(i);
        }
        s = s
            .chars()
            .take_while(|c| {
                !(0x1F3FB..=0x1F3FF).contains(&(*c as u32)) && *c != '\u{FE0F}'
            })
            .collect();
    }

    // Regional indicator pairs: fall back to ASCII country code.
    if caps.regional == Deviation::TooLong {
        if let Some((a, b)) = ri_pair(&s) {
            s = ri_to_ascii(a, b);
        }
    }

    // Keycaps: base + VS16 + U+20E3  ->  base.
    if caps.keycap == Deviation::TooLong {
        if let Some(base) = s.strip_suffix('\u{20E3}') {
            s = base.trim_end_matches('\u{FE0F}').to_string();
        }
    }

    // VS16: strip it; the base renders as text presentation.
    if caps.vs16 == Deviation::TooLong {
        s.retain(|c| c != '\u{FE0F}');
    }

    // Skin tone modifiers: strip them.
    if caps.skin_tone == Deviation::TooLong {
        s.retain(|c| !(0x1F3FB..=0x1F3FF).contains(&(c as u32)));
    }

    s
}

// ==================================================================== state


struct State {
    caps: Capabilities,
    entries: HashMap<String, Entry>,
    fingerprint: String,
    cache_path: PathBuf,
    enabled: bool,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();


const EMOJI_TEST: &str = include_str!("../assets/emoji-test.txt");

fn load_corpus() -> Vec<String> {
    let mut v = Vec::new();

    // ASCII printable — cheap, always worth having.
    for c in 0x20u8..0x7F {
        v.push((c as char).to_string());
    }

    // All fully-qualified emoji sequences.
    for line in EMOJI_TEST.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((codes, rest)) = line.split_once(';') else { continue };
        let Some((status, _)) = rest.split_once('#') else { continue };
        if status.trim() != "fully-qualified" { continue; }
        let cps: Option<Vec<u32>> = codes
            .split_whitespace()
            .map(|s| u32::from_str_radix(s, 16).ok())
            .collect();
        let Some(cps) = cps else { continue };
        let Some(s) = cps.iter().copied().map(char::from_u32).collect::<Option<String>>()
        else { continue };
        v.push(s);
    }
    v
}

fn correction(diff: i32) -> String {
    if diff > 0 {
        "\x08".repeat(diff as usize)
    } else if diff < 0 {
        " ".repeat((-diff) as usize)
    } else {
        String::new()
    }
}

fn build_emit(g: &str, caps: &Capabilities, tty: &mut Tty) -> io::Result<Entry> {
    let input_expected = g.width() as i32;
    let input_measured = tty.probe(g)?.map(|c| c as i32 - 1).unwrap_or(input_expected);
    let input_diff = input_measured - input_expected;

    let make = |emit_str: &str, exp: i32, m: i32, ie: i32, im: i32| {
    let emit = format!("{}{}", emit_str, correction(m - exp));
    Entry {
        suggested: emit.clone(),
        emit,
        expected: exp,
        measured: m,
        input_expected: ie,
        input_measured: im,
        human_reviewed: false,
    }
};

    // Rule 1: small deviation — keep as-is and pad.
    if input_diff.abs() <= PAD_THRESHOLD {
        return Ok(make(g, input_expected, input_measured,
                       input_expected, input_measured));
    }

    // Negative deviation: pad, never strip. Under-advance is a measurement
    // bug that padding fixes directly; stripping would only make it worse.
    if input_diff < 0 {
        return Ok(make(g, input_expected, input_measured,
                       input_expected, input_measured));
    }

    // Positive deviation beyond threshold: cascade.
    let stages = cascade_stages(g, caps);
    let mut best = (g.to_string(), input_expected, input_measured);

    for s in stages.iter().skip(1) {
        let exp = s.width() as i32;
        let m = tty.probe(s)?.map(|c| c as i32 - 1).unwrap_or(exp);
        let d = m - exp;

        if d.abs() <= PAD_THRESHOLD {
            return Ok(make(s, exp, m, input_expected, input_measured));
        }
        if d.abs() < (best.2 - best.1).abs() {
            best = (s.clone(), exp, m);
        }
    }

    Ok(make(&best.0, best.1, best.2, input_expected, input_measured))
}

fn build_entries(
    corpus: &[String],
    caps: &Capabilities,
    tty: &mut Tty,
) -> io::Result<HashMap<String, Entry>> {
    let total = corpus.len();
    let mut entries: HashMap<String, Entry> = HashMap::new();

    for (i, g) in corpus.iter().enumerate() {
        tty.write(&format!(
            "\x1b[1;1H\x1b[2KCalibrating {}/{}  {}",
            i + 1, total, g
        ))?;

        let entry = build_emit(g, caps, tty)?;
        if entry.emit != *g {
            entries.insert(g.clone(), entry);
        }
    }
    Ok(entries)
}

fn stdout_is_tty() -> bool {
    unsafe { libc::isatty(libc::STDOUT_FILENO) != 0 }
}

// ================================================================ calibration

fn classify(tty: &mut Tty, s: &str, expected: i32) -> io::Result<Deviation> {
    match tty.probe(s)? {
        Some(col) => Ok(Deviation::from_diff(col as i32 - 1 - expected)),
        None => Ok(Deviation::Correct),
    }
}

fn drain(tty: &mut Tty) {
    // Reads until VTIME expires with no data. In raw mode this is ~100 ms.
    let mut byte = [0u8; 1];
    let mut guard = 0;
    loop {
        match tty.file.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                guard += 1;
                if guard > 4096 { break; }
            }
            Err(_) => break,
        }
    }
}

fn probe_capabilities(tty: &mut Tty) -> io::Result<Capabilities> {
    tty.write("\x1b[1;1HChecking CPR support...")?;
    let cpr = match tty.probe("A")? {
        Some(col) => col == 2,
        None => false,
    };
    if !cpr {
        return Ok(Capabilities::disabled());
    }

    tty.write("\x1b[1;1HCalibrating capabilities...")?;
    let zwj = classify(tty, "👨‍👩‍👧‍👦", 2)?;
    let regional = classify(tty, "🇨🇦", 2)?;
    let keycap = classify(tty, "#️⃣", 2)?;
    let vs16 = classify(tty, "❤️", 2)?;
    let skin_tone = classify(tty, "👍🏽", 2)?;

    Ok(Capabilities { cpr, zwj, regional, keycap, vs16, skin_tone })
}

fn wait_for_key(tty: &mut Tty) -> io::Result<()> {
    let mut byte = [0u8; 1];
    loop {
        match tty.file.read(&mut byte) {
            Ok(0) => continue,   // VTIME expiry; keep waiting
            Ok(_) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}
// ============================================================ public API

/// Initialise uniprint. Loads a per-terminal cache if one exists; otherwise
/// probes capabilities and writes a fresh one. Must be called before the app
/// draws anything to the terminal.
pub fn init() -> io::Result<()> {
    if !stdout_is_tty() {
        let state = State {
            caps: Capabilities::disabled(),
            entries: HashMap::new(),
            fingerprint: String::new(),
            cache_path: PathBuf::new(),
            enabled: false,
        };
        STATE.set(Mutex::new(state)).ok();
        return Ok(());
    }

    let fp = fingerprint();
    let hash = fingerprint_hash(&fp);
    let dir = cache_dir().join("terminals");
    std::fs::create_dir_all(&dir)?;
    let cache_path = dir.join(format!("{}.json", hash));

    // Try existing cache.
    if let Ok(raw) = std::fs::read_to_string(&cache_path) {
        if let Ok(cf) = serde_json::from_str::<CacheFile>(&raw) {
            if cf.format_version == FORMAT_VERSION
                && cf.fingerprint == fp
                && cf.capabilities.cpr
            {
                let state = State {
                    caps: cf.capabilities,
                    entries: cf.entries,
                    fingerprint: fp,
                    cache_path,
                    enabled: true,
                };
                STATE.set(Mutex::new(state)).ok();
                return Ok(());
            }
        }
    }

    // No cache. Build one.
    let mut tty = Tty::open()?;
    tty.enter_alt()?;
    tty.enter_raw()?;
    drain(&mut tty);

    let caps = probe_capabilities(&mut tty)?;

    let entries = if caps.cpr {
        let corpus = load_corpus();
        build_entries(&corpus, &caps, &mut tty)?
    } else {
        HashMap::new()
    };

    // Report before restoring so the user sees it in the alt screen.
    tty.write(&format!(
        "\x1b[1;1H\x1b[2KCalibrated {} entries.",
        entries.len(),
    ))?;
    tty.write("\x1b[2;1H\x1b[2KPress any key to continue...")?;
    drain(&mut tty);
    wait_for_key(&mut tty)?;

    drop(tty); // Drop restores alt screen and raw mode.

    eprintln!(
        "uniprint: calibrated {} entries -> {}",
        entries.len(),
        cache_path.display()
    );

    let state = State {
        caps,
        entries,
        fingerprint: fp.clone(),
        cache_path,
        enabled: caps.cpr,
    };
    save(&state)?;
    STATE.set(Mutex::new(state)).ok();
    Ok(())
}



/// Probe the given graphemes (their sanitized forms, actually) and add them to
/// the cache. Optional — call it before the app starts drawing if you want the
/// widths to be known for a specific set of emoji.
fn render_locked(s: &str, state: &State) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for g in s.graphemes(true) {
        if let Some(entry) = state.entries.get(g) {
            out.push_str(&entry.emit);
        } else {
            out.push_str(g);
        }
    }
    out
}

pub fn entries() -> Vec<(String, Entry)> {
    STATE
        .get()
        .and_then(|lock| {
            lock.lock().ok().map(|s| {
                let mut v: Vec<(String, Entry)> = s
                    .entries
                    .iter()
                    .map(|(k, e)| (k.clone(), e.clone()))
                    .collect();
                v.sort_by(|a, b| a.0.cmp(&b.0));
                v
            })
        })
        .unwrap_or_default()
}

pub fn calibrate(graphemes: &[&str]) -> io::Result<()> {
    let Some(lock) = STATE.get() else { return Ok(()) };
    let mut state = lock.lock().unwrap();
    if !state.enabled { return Ok(()); }

    let mut tty = Tty::open()?;
    tty.enter_alt()?;
    tty.enter_raw()?;
    drain(&mut tty);

    let caps = state.caps;
    for (i, g) in graphemes.iter().enumerate() {
        tty.write(&format!(
            "\x1b[1;1H\x1b[2KCalibrating {}/{}: {}",
            i + 1, graphemes.len(), g
        ))?;
        let entry = build_emit(g, &caps, &mut tty)?;
        if entry.emit != *g {
            state.entries.insert((*g).to_string(), entry);
        }
    }

    drain(&mut tty);
    drop(tty);
    save(&state)
}

fn save(state: &State) -> io::Result<()> {
    if !state.enabled || state.cache_path.as_os_str().is_empty() {
        return Ok(());
    }
    let cf = CacheFile {
        format_version: FORMAT_VERSION,
        fingerprint: state.fingerprint.clone(),
        generated_at: now_iso8601(),
        capabilities: state.caps,
        entries: state.entries.clone(),
    };
    let json = serde_json::to_string_pretty(&cf)?;
    std::fs::write(&state.cache_path, json)
}

/// Render `s` with sanitization and per-grapheme width correction, returning
/// the bytes that should be written to the terminal.
pub fn render(s: &str) -> String {
    let Some(lock) = STATE.get() else {
        return s.to_string();
    };
    let state = match lock.lock() {
        Ok(g) => g,
        Err(_) => return s.to_string(),
    };
    if !state.enabled {
        return s.to_string();
    }
    render_locked(s, &state)
}

/// Delete the cache file for the current terminal fingerprint. Call before
/// `init()` to force a rebuild.
pub fn clear_cache() -> io::Result<()> {
    let fp = fingerprint();
    let path = terminals_dir().join(profile_filename(&fp));
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

/// Drop-in replacement for `print!`. Prints the sanitized, width-corrected
/// form of `s` to stdout.
pub fn uniprint(s: &str) {
    let out = render(s);
    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(out.as_bytes());
    let _ = stdout.flush();
}

/// Drop-in replacement for `println!`.
pub fn uniprintln(s: &str) {
    let out = render(s);
    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(out.as_bytes());
    let _ = stdout.write_all(b"\n");
    let _ = stdout.flush();
}

#[macro_export]
macro_rules! uniprint {
    ($($arg:tt)*) => {
        $crate::uniprint(&format!($($arg)*))
    };
}

#[macro_export]
macro_rules! uniprintln {
    ($($arg:tt)*) => {
        $crate::uniprintln(&format!($($arg)*))
    };
}




// ============================================================ human review

enum Key {
    Accept,
    NoCorrection,
    StripTrail,
    AddSpace,
    RemoveSpace,
    Back,
    Quit,
    Unknown,
}

fn read_key(tty: &mut Tty) -> io::Result<Key> {
    let mut b = [0u8; 1];
    loop {
        match tty.file.read(&mut b)? {
            0 => continue,
            _ => break,
        }
    }

    if b[0] != 0x1b {
        return Ok(match b[0] {
            b'\r' | b'\n'  => Key::Accept,
            b'n'           => Key::NoCorrection,
            b's'           => Key::StripTrail,
            b'a'           => Key::AddSpace,
            b'r'           => Key::RemoveSpace,
            0x7f | 0x08    => Key::Back,   // DEL or BS
            b'b'           => Key::Back,
            b'q'           => Key::Quit,
            _              => Key::Unknown,
        });
    }

    let mut seq = [0u8; 2];
    let mut got = 0;
    while got < 2 {
        match tty.file.read(&mut seq[got..])? {
            0 => break,
            n => got += n,
        }
    }
    if got == 2 && seq[0] == b'[' {
        return Ok(match seq[1] {
            b'A' => Key::StripTrail,
            b'B' => Key::NoCorrection,
            b'C' => Key::AddSpace,
            b'D' => Key::RemoveSpace,
            _    => Key::Unknown,
        });
    }
    Ok(Key::Quit)
}

fn strip_to_base(g: &str) -> String {
    g.chars()
        .find(|c| {
            let u = *c as u32;
            !(0x1F3FB..=0x1F3FF).contains(&u)
                && *c != '\u{FE0F}'
                && *c != '\u{200D}'
                && *c != '\u{20E3}'
        })
        .map(|c| c.to_string())
        .unwrap_or_else(|| g.to_string())
}

fn draw_review(
    tty: &mut Tty,
    i: usize,
    total: usize,
    input: &str,
    entry: &Entry,
) -> io::Result<()> {
    const ANNOT_COL: u16 = 64;

    tty.write("\x1b[2J\x1b[H")?;
    tty.write(&format!("\x1b[1;1H[{}/{}]  (unreviewed)", i + 1, total))?;

    tty.write("\x1b[3;1H\x1b[2KInput:      <")?;
    tty.write(input)?;
    tty.write(">")?;

    tty.write("\x1b[4;1H\x1b[2KSuggested:  <")?;
    tty.write(&entry.suggested)?;
    tty.write(">")?;
    tty.write(&format!("\x1b[{}G", ANNOT_COL))?;
    tty.write(&format!("E:{} M:{}", entry.expected, entry.measured))?;

    let label = if entry.emit == entry.suggested {
        "Current:    <"
    } else {
        "Adjusted:   <"
    };
    tty.write("\x1b[5;1H\x1b[2K")?;
    tty.write(label)?;
    tty.write(&entry.emit)?;
    tty.write(">")?;
    tty.write(&format!("\x1b[{}G", ANNOT_COL))?;
    tty.write(&format!(
        "in E:{} M:{}",
        entry.input_expected, entry.input_measured
    ))?;

    tty.write("\x1b[7;1H\x1b[2K\
[n]one  [s]trip  [a]+space  [r]-space  [enter]keep  [q]uit")?;
    tty.write("\x1b[1;1H")?;
    tty.flush()
}

pub fn human_review() -> io::Result<()> {
    let Some(lock) = STATE.get() else { return Ok(()) };
    let mut state = lock.lock().unwrap();
    if !state.enabled {
        eprintln!("uniprint: human review requires a tty and a cache");
        return Ok(());
    }

    let mut keys: Vec<String> = state
        .entries
        .iter()
        .filter(|(_, e)| !e.human_reviewed)
        .map(|(k, _)| k.clone())
        .collect();
    keys.sort();
    let total = keys.len();
    if total == 0 {
        eprintln!("uniprint: all exceptions have been reviewed");
        return Ok(());
    }

    let mut tty = Tty::open()?;
    tty.enter_alt()?;
    tty.enter_raw()?;
    drain(&mut tty);

    let mut i = 0usize;
    let mut edited: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Working copy for the current index. `None` means "load from cache".
    // Reset on any navigation, so uncommitted edits are discarded.
    let mut working: Option<(String, Entry)> = None;

    'outer: while i < total {
        let key = keys[i].clone();
        let committed = state.entries.get(&key).cloned().unwrap();

        // Working value if we have one for this key, else the committed one.
        let entry = match &working {
            Some((k, e)) if k == &key => e.clone(),
            _ => committed.clone(),
        };

        draw_review(&mut tty, i, total, &key, &entry)?;

        match read_key(&mut tty)? {
            Key::Quit => break 'outer,

            Key::Accept => {
                // Commit the working value and advance.
                let mut e = entry.clone();
                e.human_reviewed = true;
                if e.emit != committed.emit {
                    edited.insert(key.clone());
                }
                state.entries.insert(key, e);
                working = None;
                i += 1;
            }

            Key::NoCorrection => {
                let mut e = entry;
                e.emit = key.clone();
                working = Some((key.clone(), e));
            }
            Key::StripTrail => {
                let mut e = entry;
                e.emit = strip_to_base(&key);
                working = Some((key.clone(), e));
            }
            Key::AddSpace => {
                let mut e = entry;
                e.emit.push(' ');
                working = Some((key.clone(), e));
            }
            Key::RemoveSpace => {
                let mut e = entry;
                if e.emit.ends_with(' ') { e.emit.pop(); }
                working = Some((key.clone(), e));
            }

            Key::Back => {
                if i > 0 {
                    i -= 1;
                    working = None;   // discard uncommitted edits
                }
            }

            Key::Unknown => {}
        }
    }

    tty.write(&format!(
        "\x1b[1;1H\x1b[2KReviewed {} of {}, {} edited. Saving...",
        i, total, edited.len()
    ))?;
    std::thread::sleep(std::time::Duration::from_millis(400));

    drop(tty);
    save(&state)?;

    let remaining = state.entries.values().filter(|e| !e.human_reviewed).count();
    eprintln!(
        "uniprint: {} reviewed, {} edited this session, {} unreviewed remain",
        i, edited.len(), remaining
    );
    Ok(())
}