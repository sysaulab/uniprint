
# uniprint

Grapheme-aware terminal output with automatic width correction.

`uniprint` sits between your program and the terminal and fixes the one thing
every terminal emulator gets subtly wrong: the advance width of complex
graphemes. Emoji ZWJ sequences, regional-indicator flags, keycaps, VS16
sequences, and skin-tone modifiers each render at a different width depending
on the terminal — and `unicode-width` cannot tell you which. `uniprint` measures
your terminal once, caches the result, and thereafter emits the bytes that
actually line up.

## How it works

```
segment -> sanitize(caps) -> lookup(width) -> emit + correct
```

1. **Probe.** On first run, `uniprint` queries the terminal's cursor position
   report (CPR) after printing a known grapheme. The difference between the
   reported column and `unicode-width`'s prediction is the terminal's actual
   advance.
2. **Classify.** A handful of sentinel graphemes — one ZWJ family, one flag,
   one keycap, one VS16 sequence, one skin-tone emoji — determine whether the
   terminal over-advances, under-advances, or agrees with `unicode-width` for
   each *class*.
3. **Cascade.** For any grapheme the terminal renders too wide, a cascade of
   progressively simpler fallbacks is probed (VS16 stripped, skin tone removed,
   ZWJ truncated, base codepoint only). The first one that renders correctly
   wins.
4. **Emit.** At runtime, `render()` walks the input by grapheme cluster, looks
   each one up in the cache, and appends a `\x08` (backspace) or ` ` (space)
   correction so the terminal's cursor lands where the logical width says it
   should.

The probe runs once per **terminal fingerprint** and is cached to disk. No
probing happens on subsequent runs.

## Fingerprinting

Profiles are keyed by `{terminal_name}-{terminal_version}-{os_name}`, where:

- `terminal_name` comes from `TERM_PROGRAM` (iTerm, Apple_Terminal, WezTerm, …)
  and falls back to marker env vars (`KITTY_WINDOW_ID`, `WEZTERM_VERSION`,
  `GHOSTTY_VERSION`, `WT_SESSION`, `ALACRITTY_SOCKET`, `VTE_VERSION`) before
  landing on `$TERM`.
- `terminal_version` is the matching `*_VERSION` variable, or `unknown`.
- `os_name` is `std::env::consts::OS`.

Everything environment-specific — locale, tmux, session IDs, program build
hashes — is deliberately excluded, so two users running the same terminal build
on the same OS share a profile.

Profiles live in the platform data directory:

| OS      | Path                                                         |
| ------- | ------------------------------------------------------------ |
| Linux   | `~/.local/share/uniprint/terminals/`                         |
| macOS   | `~/Library/Application Support/uniprint/terminals/`          |
| Windows | `%APPDATA%\uniprint\data\terminals\`                         |

Each file is named after the (sanitized) fingerprint, e.g.
`WezTerm-20240203-110809-5046f3d6-macos.json`.

## Usage as a library

```rust
fn main() -> std::io::Result<()> {
    uniprint::init()?;

    uniprintln!("Family: 👨‍👩‍👧‍👦");
    uniprintln!("Flag:   🇨🇦");
    uniprintln!("Keycap: 1️⃣");
    uniprintln!("Thumbs: 👍🏽");
    Ok(())
}
```

Or with format arguments, via the exported macros:

```rust
uniprintln!("Score: {} {emoji}", 42, emoji = "🕺");
```

`init()` must be called before the program draws anything to the terminal — it
may take over the alternate screen for a fraction of a second on first run to
calibrate. If stdout is not a tty, `init()` is a no-op and `render()` passes
input through unchanged.

## CLI

The crate ships two binaries.

### `uniprint`

```
uniprint [--dump] [--force] [--file FILE] [--human]
```

| Flag            | Effect                                                    |
| --------------- | --------------------------------------------------------- |
| *(none)*        | Print a demo of common problem graphemes.                 |
| `--dump`        | Print every cached exception: input, emit, and metrics.   |
| `--force`       | Delete the cache for this terminal and rebuild it.        |
| `--file FILE`   | Render each line of `FILE` and show its correction.       |
| `--human`       | Interactively review and hand-edit exceptions.            |

In `--dump` output, escaped bytes are shown as `\e` (ESC) and `\b` (BS):

```
🧑🏽‍❤️‍💋‍ (🧑)   E: 2 M: 2   in E: 2 M:14

```

### `uniprint-install`

Moves calibration profiles between `./assets/profiles` and the user's profile
store. Uses the same path helper as the runtime, so the two can never disagree
about where profiles live.

```
uniprint-install [--retrieve] [--force]
```

| Flag         | Effect                                                          |
| ------------ | --------------------------------------------------------------- |
| *(none)*     | Copy `./assets/profiles/*.json` → user library.                 |
| `--retrieve` | Copy user library `*.json` → `./assets/profiles`.               |
| `--force`    | Overwrite existing files instead of skipping them.              |

Run it from the project root. This is the intended workflow for contributing a
profile:

```sh
cargo build --release
./target/release/uniprint --force          # calibrate on your terminal
./target/release/uniprint-install --retrieve
git add assets/profiles/ && git commit -m "Add profile for X"
```

## Human review

The cascade is a heuristic. When it guesses wrong, `--human` lets you fix it
by hand. Arrow keys or letters cycle through unreviewed exceptions; `[enter]`
commits, `[n]` keeps the original, `[s]` strips to the base codepoint, `[a]`
and `[r]` add or remove a trailing space, `[b]` goes back. Edits are written
back to the profile JSON, tagged `human_reviewed: true`.

It must be noted that macOS's Terminal app does not report the actual cursor position, rather it seems to report the size of buffer in bytes. iTerm2 on the other hand is much more well behaved. The human review process has been added to address this discrepancy in macOS Terminal app.

## Building

```sh
cargo build --release
```

Requires Rust 2021. Linux and macOS use `libc::termios` directly; there is no
Windows backend for raw-mode probing yet (Windows terminals without CPR are
detected and disabled cleanly).

## Testing

`assets/emoji-test.txt` is the Unicode emoji test file. Every
`fully-qualified` sequence in it is loaded into the calibration corpus.
Update it periodically from <https://unicode.org/Public/emoji/>.

## License

MIT. See [LICENSE](LICENSE).

```text
MIT License

Copyright (c) 2026 Sylvain Saucier

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```


Two things to fill in:


2. **The metrics column example** under `--dump` is illustrative; run `uniprint --dump` on your actual terminal and paste a real line so the README doesn't over-promise formatting.