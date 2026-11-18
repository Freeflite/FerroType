//! The history of copied and typed text, and the pinned entries kept from it.
//!
//! Entries are kept newest first. Pinned entries stay until they're unpinned or deleted; the
//! rest are dropped oldest first once there are more than `max_entries` of them or they add up to
//! more than `max_bytes`. What survives a restart depends on the [`Mode`].

/// What happens to the history.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    /// Nothing is recorded; only pinned entries exist (and are saved).
    Off,
    /// Strict: recorded while FerroType runs, but only pinned entries are saved.
    #[default]
    Session,
    /// Everything is saved (encrypted to your Windows account) and back after a restart.
    Saved,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Off, Mode::Session, Mode::Saved];

    pub fn id(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Session => "session",
            Mode::Saved => "saved",
        }
    }

    pub fn from_id(id: &str) -> Option<Mode> {
        Mode::ALL.into_iter().find(|mode| mode.id() == id)
    }
}

/// Where an entry came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Copied to the clipboard in some program.
    Copied,
    /// Typed by FerroType.
    Typed,
    /// Pinned straight from the editor.
    Pinned,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Stable while FerroType runs (not saved).
    pub id: u64,
    pub text: String,
    pub pinned: bool,
    pub source: Source,
    /// Seconds since the Unix epoch when it was last added.
    pub when: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Unpinned entries kept at most.
    pub max_entries: usize,
    /// The unpinned entries' text adds up to at most this many bytes (UTF-8). A single entry
    /// bigger than this isn't recorded, nor pinned.
    pub max_bytes: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { max_entries: 50, max_bytes: 2 * 1024 * 1024 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Added {
    /// A new entry with this id.
    New(u64),
    /// The same text was already there; it moved to the top.
    Moved(u64),
    Skipped(Skip),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// History is off.
    Off,
    /// Only whitespace.
    Blank,
    /// Bigger than the size limit.
    TooBig,
}

/// How much of the limits the unpinned entries use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub entries: usize,
    pub bytes: usize,
    pub pinned: usize,
}

#[derive(Debug, Default)]
pub struct History {
    entries: Vec<Entry>,
    next_id: u64,
    mode: Mode,
    limits: Limits,
}

impl History {
    pub fn new(mode: Mode, limits: Limits) -> History {
        History { entries: Vec::new(), next_id: 1, mode, limits }
    }

    /// Takes back entries read from disk (newest first), as far as the mode and limits allow.
    pub fn restore(&mut self, saved: Vec<Entry>) {
        for mut entry in saved {
            if self.entries.iter().any(|e| e.text == entry.text)
                || entry.text.len() > self.limits.max_bytes
                || (!entry.pinned && self.mode != Mode::Saved)
            {
                continue;
            }
            entry.id = self.take_id();
            self.entries.push(entry);
        }
        self.enforce();
    }

    /// Records copied or typed text, or moves it to the top if it's already there.
    pub fn add(&mut self, text: &str, source: Source, now: u64) -> Added {
        if self.mode == Mode::Off {
            return Added::Skipped(Skip::Off);
        }
        self.insert(text, source, now, false)
    }

    /// Pins `text`, adding it if it isn't there yet (even with history off).
    pub fn pin_text(&mut self, text: &str, now: u64) -> Added {
        self.insert(text, Source::Pinned, now, true)
    }

    fn insert(&mut self, text: &str, source: Source, now: u64, pin: bool) -> Added {
        if text.trim().is_empty() {
            return Added::Skipped(Skip::Blank);
        }
        if text.len() > self.limits.max_bytes {
            return Added::Skipped(Skip::TooBig);
        }
        let added = if let Some(index) = self.entries.iter().position(|e| e.text == text) {
            let mut entry = self.entries.remove(index);
            entry.when = now;
            entry.pinned |= pin;
            // A copy of something typed earlier is still worth knowing was typed.
            if entry.source != Source::Pinned {
                entry.source = source;
            }
            let id = entry.id;
            self.entries.insert(0, entry);
            Added::Moved(id)
        } else {
            let id = self.take_id();
            self.entries.insert(0, Entry { id, text: text.to_owned(), pinned: pin, source, when: now });
            Added::New(id)
        };
        self.enforce();
        added
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id.max(1);
        self.next_id = id + 1;
        id
    }

    /// Pins or unpins an entry; false if there's no such entry. An entry unpinned while history
    /// is off goes away.
    pub fn set_pinned(&mut self, id: u64, pinned: bool) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) else { return false };
        entry.pinned = pinned;
        self.enforce();
        true
    }

    pub fn remove(&mut self, id: u64) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.id != id);
        self.entries.len() != before
    }

    /// Removes every unpinned entry; returns how many went.
    pub fn clear(&mut self) -> usize {
        let before = self.entries.len();
        self.entries.retain(|e| e.pinned);
        before - self.entries.len()
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.enforce();
    }

    pub fn set_limits(&mut self, limits: Limits) {
        self.limits = limits;
        self.enforce();
    }

    pub fn get(&self, id: u64) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// For the list: pinned entries first, then the rest, each newest first.
    pub fn ordered(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| e.pinned).chain(self.entries.iter().filter(|e| !e.pinned))
    }

    pub fn usage(&self) -> Usage {
        let mut usage = Usage::default();
        for entry in &self.entries {
            if entry.pinned {
                usage.pinned += 1;
            } else {
                usage.entries += 1;
                usage.bytes += entry.text.len();
            }
        }
        usage
    }

    /// What goes to disk: everything when history is saved, otherwise only the pinned entries.
    pub fn to_save(&self) -> Vec<&Entry> {
        self.entries.iter().filter(|e| e.pinned || self.mode == Mode::Saved).collect()
    }

    /// Drops the oldest unpinned entries until the rest fit the mode and limits.
    fn enforce(&mut self) {
        if self.mode == Mode::Off {
            self.entries.retain(|e| e.pinned);
            return;
        }
        let Limits { max_entries, max_bytes } = self.limits;
        let mut count = 0;
        let mut bytes = 0;
        // Newest first, so the entries that stop fitting are the oldest.
        self.entries.retain(|e| {
            if e.pinned {
                return true;
            }
            count += 1;
            bytes += e.text.len();
            count <= max_entries && bytes <= max_bytes
        });
    }
}

// ---------------------------------------------------------------------------------------------
// File format (before encryption):
//
//   "FTH1"                     magic
//   u32 count                  little-endian, like every number here
//   count times:
//     u8  flags                bit 0 pinned; bits 1-2 source (0 copied, 1 typed, 2 pinned)
//     u64 when                 seconds since the Unix epoch
//     u32 length               of the text, in bytes
//     ... text                 UTF-8
// ---------------------------------------------------------------------------------------------

const MAGIC: &[u8; 4] = b"FTH1";
/// The smallest possible entry: flags, when, length.
const ENTRY_HEADER: usize = 1 + 8 + 4;

pub fn encode(entries: &[&Entry]) -> Vec<u8> {
    let size = 8 + entries.iter().map(|e| ENTRY_HEADER + e.text.len()).sum::<usize>();
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for entry in entries {
        let source = match entry.source {
            Source::Copied => 0,
            Source::Typed => 1,
            Source::Pinned => 2,
        };
        out.push(u8::from(entry.pinned) | (source << 1));
        out.extend_from_slice(&entry.when.to_le_bytes());
        out.extend_from_slice(&(entry.text.len() as u32).to_le_bytes());
        out.extend_from_slice(entry.text.as_bytes());
    }
    out
}

/// Reads [`encode`]'s output back; `None` for anything else (it never panics, whatever the
/// input). Entries come back with id 0; [`History::restore`] numbers them.
pub fn decode(bytes: &[u8]) -> Option<Vec<Entry>> {
    let mut reader = Reader(bytes);
    if reader.take(4)? != MAGIC {
        return None;
    }
    let count = reader.u32()? as usize;
    // Every entry needs at least a header, so a bigger count can't be honest.
    if count > reader.0.len() / ENTRY_HEADER {
        return None;
    }
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let flags = reader.take(1)?[0];
        let source = match flags >> 1 {
            0 => Source::Copied,
            1 => Source::Typed,
            2 => Source::Pinned,
            _ => return None,
        };
        let when = reader.u64()?;
        let length = reader.u32()? as usize;
        let text = std::str::from_utf8(reader.take(length)?).ok()?.to_owned();
        entries.push(Entry { id: 0, text, pinned: flags & 1 != 0, source, when });
    }
    reader.0.is_empty().then_some(entries)
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        if count > self.0.len() {
            return None;
        }
        let (head, rest) = self.0.split_at(count);
        self.0 = rest;
        Some(head)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(history: &History) -> Vec<String> {
        history.ordered().map(|e| format!("{}{}", if e.pinned { "*" } else { "" }, e.text)).collect()
    }

    fn history(max_entries: usize, max_bytes: usize) -> History {
        History::new(Mode::Session, Limits { max_entries, max_bytes })
    }

    #[test]
    fn newest_first_and_moves_repeats_up() {
        let mut h = history(10, 1000);
        assert!(matches!(h.add("one", Source::Copied, 1), Added::New(_)));
        h.add("two", Source::Copied, 2);
        assert!(matches!(h.add("one", Source::Typed, 3), Added::Moved(_)));
        assert_eq!(texts(&h), ["one", "two"]);
        assert_eq!(h.ordered().next().unwrap().source, Source::Typed);
        assert_eq!(h.add("   \n", Source::Copied, 4), Added::Skipped(Skip::Blank));
    }

    #[test]
    fn drops_oldest_unpinned_past_the_limits() {
        let mut h = history(3, 1000);
        for (i, text) in ["a", "b", "c", "d", "e"].into_iter().enumerate() {
            h.add(text, Source::Copied, i as u64);
        }
        assert_eq!(texts(&h), ["e", "d", "c"]);
        // Pinned entries don't count and are never dropped.
        let c = h.ordered().find(|e| e.text == "c").unwrap().id;
        h.set_pinned(c, true);
        h.add("f", Source::Copied, 9);
        h.add("g", Source::Copied, 10);
        h.add("h", Source::Copied, 11);
        assert_eq!(texts(&h), ["*c", "h", "g", "f"]);
        // By size: 4 bytes allowed.
        let mut h = history(100, 4);
        h.add("aa", Source::Copied, 1);
        h.add("bb", Source::Copied, 2);
        h.add("cc", Source::Copied, 3);
        assert_eq!(texts(&h), ["cc", "bb"]);
        assert_eq!(h.add("12345", Source::Copied, 4), Added::Skipped(Skip::TooBig));
        assert_eq!(h.usage(), Usage { entries: 2, bytes: 4, pinned: 0 });
    }

    #[test]
    fn modes() {
        let mut h = history(10, 1000);
        h.add("copied", Source::Copied, 1);
        h.pin_text("pinned", 2);
        assert_eq!(h.to_save().len(), 1, "session mode saves pins only");
        h.set_mode(Mode::Saved);
        assert_eq!(h.to_save().len(), 2);
        h.set_mode(Mode::Off);
        assert_eq!(texts(&h), ["*pinned"]);
        assert_eq!(h.add("x", Source::Copied, 3), Added::Skipped(Skip::Off));
        assert!(matches!(h.pin_text("y", 4), Added::New(_)), "pinning works with history off");
        let y = h.ordered().find(|e| e.text == "y").unwrap().id;
        h.set_pinned(y, false);
        assert_eq!(texts(&h), ["*pinned"], "unpinned with history off = gone");
    }

    #[test]
    fn clear_keeps_pins_and_remove_by_id() {
        let mut h = history(10, 1000);
        h.add("a", Source::Copied, 1);
        let Added::New(b) = h.pin_text("b", 2) else { panic!() };
        h.add("c", Source::Typed, 3);
        assert_eq!(h.clear(), 2);
        assert_eq!(texts(&h), ["*b"]);
        assert!(h.remove(b));
        assert!(!h.remove(b));
        assert_eq!(h.usage(), Usage::default());
    }

    #[test]
    fn round_trips_and_restores_by_mode() {
        let mut h = history(10, 1000);
        h.set_mode(Mode::Saved);
        h.add("plain", Source::Copied, 11);
        h.add("Grüße 🦀\nline two", Source::Typed, 12);
        h.pin_text("pin", 13);
        let bytes = encode(&h.to_save());
        let back = decode(&bytes).unwrap();
        assert_eq!(back.len(), 3);
        assert_eq!(back[0].text, "pin");
        assert!(back[0].pinned && back[0].source == Source::Pinned && back[0].when == 13);
        assert_eq!(back[1].text, "Grüße 🦀\nline two");

        let mut saved = History::new(Mode::Saved, Limits::default());
        saved.restore(back.clone());
        assert_eq!(texts(&saved), ["*pin", "Grüße 🦀\nline two", "plain"]);
        let mut strict = History::new(Mode::Session, Limits::default());
        strict.restore(back);
        assert_eq!(texts(&strict), ["*pin"], "strict mode never brings back unpinned history");
        assert!(strict.ordered().all(|e| e.id != 0));
    }

    #[test]
    fn decode_rejects_damage_without_panicking() {
        let mut h = History::new(Mode::Saved, Limits::default());
        h.add("hello", Source::Copied, 1);
        h.add("world", Source::Typed, 2);
        let good = encode(&h.to_save());
        assert!(decode(&good).is_some());
        assert_eq!(decode(b""), None);
        assert_eq!(decode(b"FTH2\0\0\0\0"), None);
        for cut in 0..good.len() {
            assert_eq!(decode(&good[..cut]), None, "cut at {cut}");
        }
        let mut extra = good.clone();
        extra.push(0);
        assert_eq!(decode(&extra), None);
        // Huge count, bad source, invalid UTF-8.
        assert_eq!(decode(b"FTH1\xff\xff\xff\xff"), None);
        let mut bad_source = good.clone();
        bad_source[8] = 0b110;
        assert_eq!(decode(&bad_source), None);
        let mut bad_utf8 = good.clone();
        let at = good.len() - 1;
        bad_utf8[at] = 0xff;
        assert_eq!(decode(&bad_utf8), None);
        // Every single-byte change either decodes or is rejected, never panics.
        for i in 0..good.len() {
            for value in [0u8, 1, 0x7f, 0x80, 0xff] {
                let mut copy = good.clone();
                copy[i] = value;
                let _ = decode(&copy);
            }
        }
    }
}
