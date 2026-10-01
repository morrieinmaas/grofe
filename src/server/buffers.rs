//! Paste buffers (TM.4, spec §4.11), as tmux 3.7b's `paste.c` keeps them.
//!
//! - An automatic buffer is named `buffer<N>` from a counter that only
//!   grows, so `buffer0` is the oldest still kept; a name already taken is
//!   skipped. Adding one when `buffer-limit` automatic buffers exist drops
//!   the oldest automatic one first. Named buffers never count.
//! - Every write to a buffer by name (`set-buffer -b`, `-a` onto it) and
//!   every rename makes it a named buffer, as tmux does. A write puts the
//!   buffer first; a rename keeps its place.
//! - Lists run newest first. With no name, a command uses the newest
//!   automatic buffer ([`Buffers::top`]).
//!
//! Each buffer is bounded ([`MAX_BUFFER_BYTES`]) and so is the sum of all
//! of them with their names and a fixed [`OVERHEAD`] each
//! ([`MAX_TOTAL_BYTES`]), so a hostile caller can grow the server's memory
//! only so far; tmux bounds neither. A name with a control character is
//! refused, as tmux does, and a name past [`MAX_NAME_BYTES`] too. A paste is
//! made by [`paste_bytes`], bounded by [`MAX_PASTE_BYTES`].

use std::time::{SystemTime, UNIX_EPOCH};

use crate::cmd::FormatVars;
use crate::server::State;

/// One buffer's largest size. A paste of it fits a pane's input queue
/// (`pane::io`'s 4 MiB budget) when nothing else is queued.
pub const MAX_BUFFER_BYTES: usize = 4 << 20;
/// The most every buffer's data and name may add up to.
pub const MAX_TOTAL_BYTES: usize = 64 << 20;
/// The most one paste may write, separators and escapes included.
pub const MAX_PASTE_BYTES: usize = 16 << 20;
/// What each buffer costs beyond its name and data.
pub const OVERHEAD: usize = 64;
/// The longest buffer name (tmux has no limit).
pub const MAX_NAME_BYTES: usize = 1024;
/// tmux's `buffer-limit` default.
pub const DEFAULT_LIMIT: usize = 50;
/// How much of a buffer `buffer_sample` shows (tmux's `paste_make_sample`).
const SAMPLE_WIDTH: usize = 200;

/// tmux 3.7b's `list-buffers` template.
pub const LIST_FORMAT: &str = "#{buffer_name}: #{buffer_size} bytes: \"#{buffer_sample}\"";

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Buffer {
    /// Unique for the server's life, kept through a rename (a chooser's
    /// line finds its buffer again by it).
    pub id: u64,
    pub name: String,
    pub data: Vec<u8>,
    /// Unix seconds (`buffer_created`).
    pub created: u64,
    automatic: bool,
}

impl Buffer {
    /// What the buffer counts against [`MAX_TOTAL_BYTES`]: its name, its
    /// data and [`OVERHEAD`], so many tiny buffers count too.
    fn cost(&self) -> usize {
        OVERHEAD + self.name.len() + self.data.len()
    }
}

/// Every buffer, newest first. Lives on `State.buffers`.
#[derive(Debug, Default)]
pub struct Buffers {
    list: Vec<Buffer>,
    next_index: u64,
    next_id: u64,
    total: usize,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// tmux 3.7b's check on a name a buffer is given (`-b`, `-n`): no control
/// character (below 0x20, or DEL); grofe also bounds its length.
fn check_name(name: &str) -> Result<(), String> {
    if name.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(format!("invalid buffer name: {name}"));
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(format!(
            "buffer name too long: {} bytes (limit {MAX_NAME_BYTES})",
            name.len()
        ));
    }
    Ok(())
}

fn too_large(len: usize) -> String {
    format!("buffer too large: {len} bytes (limit {MAX_BUFFER_BYTES})")
}

impl Buffers {
    /// Newest first.
    pub fn iter(&self) -> impl Iterator<Item = &Buffer> {
        self.list.iter()
    }

    /// The buffer called `name`; an empty name is none, as in tmux.
    pub fn get(&self, name: &str) -> Option<&Buffer> {
        if name.is_empty() {
            return None;
        }
        self.list.iter().find(|b| b.name == name)
    }

    /// The buffer with this [`Buffer::id`].
    pub fn by_id(&self, id: u64) -> Option<&Buffer> {
        self.list.iter().find(|b| b.id == id)
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// The newest automatic buffer.
    pub fn top(&self) -> Option<&Buffer> {
        self.list.iter().find(|b| b.automatic)
    }

    /// Adds a new automatic buffer, dropping the oldest automatic ones
    /// while `limit` (at least 1) or more exist, and while the total would
    /// pass [`MAX_TOTAL_BYTES`]. Empty data adds nothing.
    pub fn add(&mut self, data: Vec<u8>, limit: usize) -> Result<(), String> {
        if data.is_empty() {
            return Ok(());
        }
        if data.len() > MAX_BUFFER_BYTES {
            return Err(too_large(data.len()));
        }
        while self.list.iter().filter(|b| b.automatic).count() >= limit.max(1) {
            self.drop_oldest_automatic();
        }
        let name = loop {
            let name = format!("buffer{}", self.next_index);
            self.next_index += 1;
            if self.get(&name).is_none() {
                break name;
            }
        };
        let buffer = Buffer {
            id: 0,
            name,
            data,
            created: now(),
            automatic: true,
        };
        while self.total + buffer.cost() > MAX_TOTAL_BYTES {
            if !self.drop_oldest_automatic() {
                return Err("buffers full".to_string());
            }
        }
        self.push(buffer);
        Ok(())
    }

    /// Sets the named buffer `name` to `data`, replacing any buffer of
    /// that name; it becomes the newest. Empty data changes nothing.
    pub fn set(&mut self, name: &str, data: Vec<u8>) -> Result<(), String> {
        if name.is_empty() {
            return Err("empty buffer name".to_string());
        }
        check_name(name)?;
        if data.is_empty() {
            return Ok(());
        }
        if data.len() > MAX_BUFFER_BYTES {
            return Err(too_large(data.len()));
        }
        let old = self.get(name).map_or(0, Buffer::cost);
        let buffer = Buffer {
            id: 0,
            name: name.to_string(),
            data,
            created: now(),
            automatic: false,
        };
        if self.total - old + buffer.cost() > MAX_TOTAL_BYTES {
            return Err("buffers full".to_string());
        }
        self.delete(name);
        self.push(buffer);
        Ok(())
    }

    /// Renames `old` to `new`, replacing a buffer called `new`; the
    /// renamed buffer keeps its place and becomes a named one. tmux's
    /// `paste_rename` and its errors.
    pub fn rename(&mut self, old: &str, new: &str) -> Result<(), String> {
        if old.is_empty() {
            return Err("no buffer".to_string());
        }
        if new.is_empty() {
            return Err("new name is empty".to_string());
        }
        check_name(new)?;
        if self.get(old).is_none() {
            return Err(format!("no buffer {old}"));
        }
        if old == new {
            return Ok(());
        }
        let replaced = self.get(new).map_or(0, Buffer::cost);
        if self.total - replaced - old.len() + new.len() > MAX_TOTAL_BYTES {
            return Err("buffers full".to_string());
        }
        self.delete(new);
        if let Some(b) = self.list.iter_mut().find(|b| b.name == old) {
            self.total = self.total - old.len() + new.len();
            b.name = new.to_string();
            b.automatic = false;
        }
        Ok(())
    }

    /// Deletes the buffer called `name`; whether there was one.
    pub fn delete(&mut self, name: &str) -> bool {
        let Some(i) = self.list.iter().position(|b| b.name == name) else {
            return false;
        };
        let b = self.list.remove(i);
        self.total -= b.cost();
        true
    }

    fn push(&mut self, mut buffer: Buffer) {
        buffer.id = self.next_id;
        self.next_id += 1;
        self.total += buffer.cost();
        self.list.insert(0, buffer);
    }

    fn drop_oldest_automatic(&mut self) -> bool {
        let Some(i) = self.list.iter().rposition(|b| b.automatic) else {
            return false;
        };
        let b = self.list.remove(i);
        self.total -= b.cost();
        true
    }
}

/// `buffer-limit`, at least 1.
pub fn limit(state: &State) -> usize {
    state
        .model
        .options
        .lookup("buffer-limit", &[])
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_LIMIT)
        .max(1)
}

/// One buffer's format variables (`list-buffers -F`), and nothing else:
/// tmux 3.7b gives a buffer's format no session or pane (recorded:
/// `#{session_name}` is empty there).
pub struct BufferVars<'a>(pub &'a Buffer);

impl FormatVars for BufferVars<'_> {
    fn get(&self, name: &str) -> Option<String> {
        let b = self.0;
        match name {
            "buffer_name" => Some(b.name.clone()),
            "buffer_size" => Some(b.data.len().to_string()),
            "buffer_sample" => Some(sample(&b.data)),
            "buffer_created" => Some(b.created.to_string()),
            _ => None,
        }
    }
}

/// `buffer_*` in any other format: the newest automatic buffer's, as tmux
/// adds it to every format; nothing when there is none.
pub fn format_var(state: &State, name: &str) -> Option<String> {
    if !name.starts_with("buffer_") {
        return None;
    }
    let top = state.buffers.top()?;
    BufferVars(top).get(name)
}

/// tmux's `paste_make_sample`: the first 200 bytes, escaped as `vis(3)`
/// with `VIS_OCTAL | VIS_CSTYLE | VIS_TAB | VIS_NL` (valid UTF-8 kept
/// whole), cut to 200 bytes with `...` when the buffer or its escaped
/// form is longer.
// ponytail: the cut lands on a character boundary at or before byte 200,
// where tmux cuts at byte 200 exactly; they differ only when a character
// of more than one byte straddles it.
pub fn sample(data: &[u8]) -> String {
    let mut out = escape(&data[..data.len().min(SAMPLE_WIDTH)]);
    if data.len() > SAMPLE_WIDTH || out.len() > SAMPLE_WIDTH {
        let mut cut = SAMPLE_WIDTH.min(out.len());
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        out.push_str("...");
    }
    out
}

/// `data` escaped as `vis(3)` with `VIS_OCTAL | VIS_CSTYLE | VIS_TAB |
/// VIS_NL`, valid UTF-8 kept whole (tmux's `utf8_strvis`): what a
/// sample, and each line of `choose-buffer`'s preview, shows.
pub fn escape(data: &[u8]) -> String {
    let mut out = String::new();
    for chunk in data.utf8_chunks() {
        let mut chars = chunk.valid().chars().peekable();
        while let Some(c) = chars.next() {
            let next = chars.peek().copied();
            cstyle(c, next, &mut out);
        }
        for b in chunk.invalid() {
            out.push_str(&format!("\\{b:03o}"));
        }
    }
    out
}

/// One character as `vis(3)` writes it with `VIS_CSTYLE | VIS_OCTAL`.
fn cstyle(c: char, next: Option<char>, out: &mut String) {
    match c {
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\x08' => out.push_str("\\b"),
        '\x07' => out.push_str("\\a"),
        '\x0b' => out.push_str("\\v"),
        '\t' => out.push_str("\\t"),
        '\x0c' => out.push_str("\\f"),
        '\\' => out.push_str("\\\\"),
        '\0' if next.is_some_and(|n| ('0'..='7').contains(&n)) => out.push_str("\\000"),
        '\0' => out.push_str("\\0"),
        c if c.is_ascii_control() => out.push_str(&format!("\\{:03o}", c as u32)),
        c => out.push(c),
    }
}

/// What `paste-buffer` writes to a pane for `data`: each line feed
/// replaced by `separator`, every line sanitized as tmux 3.7b does
/// ([`sanitize_line`]), wrapped in bracketed-paste markers when `bracket`
/// (`-p` and a pane in mode 2004). Since sanitizing turns an escape in
/// the data into the two characters `^[`, the data cannot end the
/// bracketed paste early or send the pane any other control sequence.
/// Without `sanitize` (`-S`, the caller's explicit choice, as in tmux) the
/// lines go as they are. Errs past [`MAX_PASTE_BYTES`].
pub fn paste_bytes(
    data: &[u8],
    separator: &[u8],
    bracket: bool,
    sanitize: bool,
) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    if bracket {
        out.extend_from_slice(PASTE_START);
    }
    for (i, line) in data.split(|b| *b == b'\n').enumerate() {
        if i > 0 {
            out.extend_from_slice(separator);
        }
        if sanitize {
            sanitize_line(line, &mut out);
        } else {
            out.extend_from_slice(line);
        }
        if out.len() > MAX_PASTE_BYTES {
            return Err(format!("paste too large (limit {MAX_PASTE_BYTES} bytes)"));
        }
    }
    if bracket {
        out.extend_from_slice(PASTE_END);
    }
    Ok(out)
}

/// tmux 3.7b's paste sanitizing, `vis(3)` with `VIS_SAFE | VIS_NOSLASH`
/// (`utf8_stravisx`): valid UTF-8 and printable ASCII stay, and so do tab,
/// line feed, backspace, bell and carriage return; any other control
/// character becomes `^X` (DEL `^?`), and a byte that is not UTF-8 `M-x`
/// or `M^X`.
// ponytail: an invalid byte in 0xa1-0xff is `M-x` here; tmux on macOS
// passes it through and glibc's answer depends on the locale. `set-buffer`
// cannot make such a buffer (its data is a UTF-8 argument).
fn sanitize_line(line: &[u8], out: &mut Vec<u8>) {
    let caret = |c: u8, out: &mut Vec<u8>| {
        out.push(b'^');
        out.push(if c == 0x7f { b'?' } else { c + b'@' });
    };
    for chunk in line.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '\t' | '\n' | '\x08' | '\x07' | '\r' => out.push(c as u8),
                c if c.is_ascii_control() => caret(c as u8, out),
                c => {
                    let mut buf = [0; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
            }
        }
        for &b in chunk.invalid() {
            let low = b & 0x7f;
            if low == b' ' {
                out.extend_from_slice(format!("\\{b:03o}").as_bytes());
            } else if low.is_ascii_control() {
                out.push(b'M');
                caret(low, out);
            } else {
                out.extend_from_slice(&[b'M', b'-', low]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(b: &Buffers) -> Vec<&str> {
        b.iter().map(|b| b.name.as_str()).collect()
    }

    fn data(b: &Buffers, name: &str) -> String {
        String::from_utf8(b.get(name).unwrap().data.clone()).unwrap()
    }

    /// Recorded on tmux 3.7b: names count up, the list runs newest first.
    #[test]
    fn automatic_buffers_count_up_and_list_newest_first() {
        let mut b = Buffers::default();
        for d in ["one", "two", "three"] {
            b.add(d.into(), 50).unwrap();
        }
        assert_eq!(names(&b), ["buffer2", "buffer1", "buffer0"]);
        assert_eq!(b.top().unwrap().name, "buffer2");
        b.add(Vec::new(), 50).unwrap();
        assert_eq!(names(&b).len(), 3, "empty data adds nothing");
    }

    #[test]
    fn the_limit_drops_the_oldest_automatic_buffer_only() {
        let mut b = Buffers::default();
        b.set("keep", b"k".to_vec()).unwrap();
        for i in 0..51 {
            b.add(format!("d{i}").into_bytes(), 50).unwrap();
        }
        assert_eq!(b.iter().filter(|b| b.automatic).count(), 50);
        assert!(b.get("buffer0").is_none());
        assert_eq!(data(&b, "buffer1"), "d1");
        assert_eq!(data(&b, "keep"), "k");
        // Recorded: lowering the limit takes effect on the next add.
        b.add(b"x".to_vec(), 1).unwrap();
        assert_eq!(names(&b), ["buffer51", "keep"]);
    }

    #[test]
    fn a_taken_name_is_skipped() {
        let mut b = Buffers::default();
        b.set("buffer0", b"mine".to_vec()).unwrap();
        b.add(b"a".to_vec(), 50).unwrap();
        assert_eq!(names(&b), ["buffer1", "buffer0"]);
        assert_eq!(data(&b, "buffer0"), "mine");
    }

    /// Recorded: a write by name makes the buffer named and newest; a
    /// rename keeps its place, replaces a buffer of the new name and makes
    /// it named.
    #[test]
    fn writes_and_renames_make_named_buffers() {
        let mut b = Buffers::default();
        for d in ["a0", "a1", "a2"] {
            b.add(d.into(), 50).unwrap();
        }
        b.set("buffer0", b"a0X".to_vec()).unwrap();
        assert_eq!(names(&b), ["buffer0", "buffer2", "buffer1"]);
        assert_eq!(b.top().unwrap().name, "buffer2");
        b.rename("buffer1", "foo").unwrap();
        assert_eq!(names(&b), ["buffer0", "buffer2", "foo"]);
        b.rename("foo", "buffer2").unwrap();
        assert_eq!(names(&b), ["buffer0", "buffer2"]);
        assert_eq!(data(&b, "buffer2"), "a1");
        assert!(b.top().is_none());
        assert_eq!(b.rename("nob", "x"), Err("no buffer nob".to_string()));
        assert_eq!(
            b.rename("buffer0", ""),
            Err("new name is empty".to_string())
        );
    }

    /// Recorded on tmux 3.7b: a control character (ESC, tab, DEL, LF) in
    /// a new name is refused, for a write and a rename; UTF-8, C1 and
    /// spaces are fine.
    #[test]
    fn names_with_control_characters_are_refused() {
        let mut b = Buffers::default();
        for name in ["a\x1bb", "a\tb", "a\x7fb", "a\nb", "a\x01"] {
            assert_eq!(
                b.set(name, b"x".to_vec()),
                Err(format!("invalid buffer name: {name}"))
            );
        }
        for name in ["a b", "é", "a\u{9b}b"] {
            b.set(name, b"x".to_vec()).unwrap();
        }
        assert_eq!(
            b.rename("a b", "a\x1bb"),
            Err("invalid buffer name: a\x1bb".to_string())
        );
        assert!(b.get("a b").is_some());
        let long = "n".repeat(MAX_NAME_BYTES + 1);
        assert!(
            b.set(&long, b"x".to_vec())
                .unwrap_err()
                .contains("too long")
        );
        assert!(b.rename("a b", &long).unwrap_err().contains("too long"));
        b.set(&long[1..], b"x".to_vec()).unwrap();
    }

    /// A rename that would pass the total is refused, and leaves both the
    /// buffer and any buffer of the new name alone; tiny buffers count
    /// their overhead.
    #[test]
    fn renames_and_tiny_buffers_count_against_the_total() {
        let mut b = Buffers::default();
        let name = "n".repeat(MAX_NAME_BYTES);
        let mut i = 0;
        let full = vec![b'x'; MAX_BUFFER_BYTES];
        while b.total + MAX_BUFFER_BYTES + OVERHEAD + 8 < MAX_TOTAL_BYTES {
            b.set(&format!("f{i}"), full.clone()).unwrap();
            i += 1;
        }
        let room = MAX_TOTAL_BYTES - b.total;
        let fill = room - OVERHEAD - 1 - 10;
        b.set("s", vec![b'y'; fill]).unwrap();
        assert!(MAX_TOTAL_BYTES - b.total < MAX_NAME_BYTES);
        assert_eq!(b.rename("s", &name), Err("buffers full".to_string()));
        assert!(b.get("s").is_some());
        b.rename("s", "short").unwrap();
        let sum: usize = b.iter().map(Buffer::cost).sum();
        assert_eq!(sum, b.total);

        let mut t = Buffers::default();
        t.set("a", b"x".to_vec()).unwrap();
        t.add(b"y".to_vec(), 50).unwrap();
        assert_eq!(t.total, 2 * OVERHEAD + 2 + "buffer0".len() + 1);
    }

    #[test]
    fn sizes_are_bounded() {
        let mut b = Buffers::default();
        let big = vec![b'x'; MAX_BUFFER_BYTES + 1];
        assert!(b.add(big.clone(), 50).is_err());
        assert!(b.set("n", big).is_err());
        let full = vec![b'x'; MAX_BUFFER_BYTES];
        for i in 0..MAX_TOTAL_BYTES / MAX_BUFFER_BYTES - 1 {
            b.set(&format!("n{i}"), full.clone()).unwrap();
        }
        assert_eq!(b.set("last", full.clone()), Err("buffers full".into()));
        // An automatic add drops older automatic buffers to fit, and fails
        // only when named ones fill the space.
        b.add(vec![b'y'; MAX_BUFFER_BYTES - 2000], 50).unwrap();
        b.add(vec![b'z'; MAX_BUFFER_BYTES - 2000], 50).unwrap();
        assert_eq!(b.iter().filter(|b| b.automatic).count(), 1);
        assert!(b.total <= MAX_TOTAL_BYTES);
        b.set("n0", b"small".to_vec()).unwrap();
        assert!(b.total <= MAX_TOTAL_BYTES);
        let sum: usize = b.iter().map(Buffer::cost).sum();
        assert_eq!(sum, b.total);
    }

    /// Recorded on tmux 3.7b (`list-buffers`).
    #[test]
    fn samples_escape_as_tmux() {
        assert_eq!(sample(b"three\nlines"), "three\\nlines");
        assert_eq!(
            sample(b"\x01\x02\x07\x08\t\n\x0b\x0c\r\x0e\x1b\x1f \x7f\x80\xffx\xc3"),
            "\\001\\002\\a\\b\\t\\n\\v\\f\\r\\016\\033\\037 \\177\\200\\377x\\303"
        );
        assert_eq!(sample(b"a\x011"), "a\\0011");
        assert_eq!(sample("a\\e \"q\" é日本".as_bytes()), "a\\\\e \"q\" é日本");
        let x = sample(&[b'x'; 300]);
        assert_eq!(x, format!("{}...", "x".repeat(200)));
        assert_eq!(sample(&[b'x'; 200]), "x".repeat(200));
        let cjk = "日本語".repeat(30);
        assert_eq!(
            sample(cjk.as_bytes()),
            format!("{}\\3...", "日本語".repeat(22))
        );
    }

    /// Recorded on tmux 3.7b, the bytes a pane reads.
    #[test]
    fn pastes_sanitize_and_separate_as_tmux() {
        let data = b"a\x1b[201~b\tc\x01d\x7fe\x08f\x07g\rh \xc3\xa9\nz";
        assert_eq!(
            paste_bytes(data, b"\r", true, true).unwrap(),
            b"\x1b[200~a^[[201~b\tc^Ad^?e\x08f\x07g\rh \xc3\xa9\rz\x1b[201~"
        );
        assert_eq!(
            paste_bytes(data, b"\r", false, true).unwrap(),
            b"a^[[201~b\tc^Ad^?e\x08f\x07g\rh \xc3\xa9\rz"
        );
        assert_eq!(
            paste_bytes(b"l1\nl2\n", b"XX", false, true).unwrap(),
            b"l1XXl2XX"
        );
        assert_eq!(
            paste_bytes(b"l1\nl2", b"\n", false, true).unwrap(),
            b"l1\nl2"
        );
        assert_eq!(
            paste_bytes(b"\x80|\x9b|\xa0|\xe9", b"\r", false, true).unwrap(),
            b"M^@|M^[|\\240|M-i"
        );
        // Recorded: `-S` sends the lines as they are, separators still
        // replacing line feeds.
        assert_eq!(
            paste_bytes(data, b"\r", true, false).unwrap(),
            b"\x1b[200~a\x1b[201~b\tc\x01d\x7fe\x08f\x07g\rh \xc3\xa9\rz\x1b[201~"
        );
        let many = vec![b'\n'; 1000];
        assert!(paste_bytes(&many, &vec![b'x'; MAX_PASTE_BYTES / 999 + 1], false, true).is_err());
    }
}
