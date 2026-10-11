//! Sensitive-data detection + tokenization (dev-plan 55, steps 1-2).
//!
//! Layer-1 rule detectors with validators (Thai national ID checksum, phone,
//! plate, name-by-prefix) plus a user-supplied custom dictionary. Detected
//! spans are pseudonymized to `[TYPE_N]` placeholders with a per-session
//! coreference map, and restored on the way back (`detokenize`).
//!
//! Router integration (step 3) and Layer-2 NER for free address/name (step 4)
//! land separately — this module is pure logic, unit-tested in isolation.
//!
//! ## Precision is the constraint, not recall
//!
//! Every span this module returns gets REPLACED in the text that goes to the
//! model. A false positive doesn't just over-mask — Thai is written without
//! word spaces, so a loose pattern swallows whole clauses and the prompt
//! arrives mangled. The first cut of these rules produced 119 hits on five
//! PII-free Thai chapters, so each detector now carries a boundary or a
//! context requirement (see `negative_corpus_has_no_detections`, which pins
//! this against real prose):
//!
//! - **name** — titled forms only (`นาย`/`นาง`/`นางสาว`/`น.ส.`/`ด.ช.`/`ด.ญ.`),
//!   must start at a word boundary, compound words blacklisted. `คุณ` is NOT a
//!   trigger: it's the everyday second-person pronoun (80 hits in the same
//!   corpus), so honorific-`คุณ` names are left to Layer-2 NER, which the plan
//!   already assigns to name/address.
//! - **plate** — requires context (`ทะเบียน…` before, or a province name
//!   after). A bare 2-3 consonants + number is ordinary Thai ("ครบ 3 ปี").
//! - **phone / id** — rejected when a digit sits adjacent, so the pattern
//!   can't carve a phone out of a bank-account number.
//! - **Thai numerals** are normalized to ASCII before matching (`๐๘๑…` is a
//!   real phone/ID and used to slip through un-tokenized — a fail-OPEN leak,
//!   the one failure mode this module must never have).

use std::collections::HashMap;
use std::fmt;
use std::sync::LazyLock;

use regex::Regex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PiiType {
    ThaiId,
    Phone,
    Plate,
    Name,
    Custom,
}

impl PiiType {
    /// Placeholder tag, e.g. `ID` → `[ID_1]`.
    fn tag(self) -> &'static str {
        match self {
            PiiType::ThaiId => "ID",
            PiiType::Phone => "PHONE",
            PiiType::Plate => "PLATE",
            PiiType::Name => "NAME",
            PiiType::Custom => "CUST",
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Span {
    pub start: usize, // byte offset
    pub end: usize,
    pub kind: PiiType,
    pub value: String,
}

/// Hand-written so the real value can't reach a log through `{:?}` — the
/// whole point of this module is that the plaintext stays local, and step 3
/// will be tracing spans as it wires the router.
impl fmt::Debug for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Span")
            .field("kind", &self.kind)
            .field("range", &(self.start..self.end))
            .field("len", &self.value.len())
            .finish()
    }
}

// 13 digits, bare or in the printed 1-4-5-2-1 grouping with a space or dash
// between groups (1-1007-00123-45-5); the checksum decides whether it is a
// real Thai ID.
static RE_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d[ -]?\d{4}[ -]?\d{5}[ -]?\d{2}[ -]?\d").unwrap());
// Thai mobile: 0[689] + 8 more digits; separators (space/dash) may fall between
// any digits (0812345678, 081-234-5678, 08-1234-5678, +66 81 234 5678).
static RE_PHONE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:\+66[\s-]?|0)[689](?:[\s-]?\d){8}").unwrap());
// Thai plate: optional leading digit (post-2012 format) + 1-3 Thai consonants +
// 1-4 digits. Deliberately loose — `plate_has_context` is what makes it a
// plate rather than any Thai word followed by a number.
static RE_PLATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:\d\s?)?[\u{0E01}-\u{0E2E}]{1,3}[\s-]?\d{1,4}").unwrap());
// Title + following token(s). `\s?` because Thai writes `นายสมชาย` as often as
// `นาย สมชาย`; the 2-20 char cap keeps a glued match from eating the clause.
static RE_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:นาย|นางสาว|นาง|น\.ส\.|ด\.ช\.|ด\.ญ\.)\s?[\u{0E01}-\u{0E4E}]{2,20}(?:\s[\u{0E01}-\u{0E4E}]{2,20})?",
    )
    .unwrap()
});
static RE_PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[[A-Z]+_\d+\]").unwrap());

/// Compounds that start with a title but are ordinary words, not names.
/// Only reachable in the glued form (`นายกรัฐมนตรี`) — `นาย สมชาย` never
/// matches one of these.
const NAME_COMPOUNDS: &[&str] = &[
    "นายก",
    "นายจ้าง",
    "นายหน้า",
    "นายทุน",
    "นายทะเบียน",
    "นายอำเภอ",
    "นายพล",
    "นายเรือ",
    "นายแพทย์",
    "นายช่าง",
    "นางแบบ",
    "นางเอก",
    "นางฟ้า",
    "นางงาม",
    "นางพยาบาล",
    "นางรำ",
    "นางสงกรานต์",
];

/// Words that make a nearby consonant+number run a vehicle plate.
const PLATE_MARKERS: &[&str] = &["ทะเบียน", "ป้ายแดง", "เลขทะเบียน"];

/// Province names as they appear on a plate (the plan's "+ จังหวัด" signal).
const PROVINCES: &[&str] = &[
    "กรุงเทพมหานคร",
    "กรุงเทพฯ",
    "กทม",
    "กระบี่",
    "กาญจนบุรี",
    "กาฬสินธุ์",
    "กำแพงเพชร",
    "ขอนแก่น",
    "จันทบุรี",
    "ฉะเชิงเทรา",
    "ชลบุรี",
    "ชัยนาท",
    "ชัยภูมิ",
    "ชุมพร",
    "เชียงราย",
    "เชียงใหม่",
    "ตรัง",
    "ตราด",
    "ตาก",
    "นครนายก",
    "นครปฐม",
    "นครพนม",
    "นครราชสีมา",
    "นครศรีธรรมราช",
    "นครสวรรค์",
    "นนทบุรี",
    "นราธิวาส",
    "น่าน",
    "บึงกาฬ",
    "บุรีรัมย์",
    "ปทุมธานี",
    "ประจวบคีรีขันธ์",
    "ปราจีนบุรี",
    "ปัตตานี",
    "พระนครศรีอยุธยา",
    "อยุธยา",
    "พังงา",
    "พัทลุง",
    "พิจิตร",
    "พิษณุโลก",
    "เพชรบุรี",
    "เพชรบูรณ์",
    "แพร่",
    "พะเยา",
    "ภูเก็ต",
    "มหาสารคาม",
    "มุกดาหาร",
    "แม่ฮ่องสอน",
    "ยะลา",
    "ยโสธร",
    "ร้อยเอ็ด",
    "ระนอง",
    "ระยอง",
    "ราชบุรี",
    "ลพบุรี",
    "ลำปาง",
    "ลำพูน",
    "เลย",
    "ศรีสะเกษ",
    "สกลนคร",
    "สงขลา",
    "สตูล",
    "สมุทรปราการ",
    "สมุทรสงคราม",
    "สมุทรสาคร",
    "สระแก้ว",
    "สระบุรี",
    "สิงห์บุรี",
    "สุโขทัย",
    "สุพรรณบุรี",
    "สุราษฎร์ธานี",
    "สุรินทร์",
    "หนองคาย",
    "หนองบัวลำภู",
    "อ่างทอง",
    "อำนาจเจริญ",
    "อุดรธานี",
    "อุตรดิตถ์",
    "อุทัยธานี",
    "อุบลราชธานี",
];

/// Thai national-ID checksum: Σ dᵢ·(13−i) mod 11, check = (11 − that) mod 10.
pub fn thai_id_valid(d: &str) -> bool {
    let b = d.as_bytes();
    if b.len() != 13 || !b.iter().all(u8::is_ascii_digit) {
        return false;
    }
    let sum: u32 = (0..12)
        .map(|i| (b[i] - b'0') as u32 * (13 - i as u32))
        .sum();
    let check = ((11 - (sum % 11)) % 10) as u8;
    (b[12] - b'0') == check
}

/// ASCII digit for a Thai numeral (`๐`-`๙`).
fn thai_digit(c: char) -> Option<char> {
    ('\u{0E50}'..='\u{0E59}')
        .contains(&c)
        .then(|| char::from(b'0' + (c as u32 - 0x0E50) as u8))
}

fn is_thai_letter(c: char) -> bool {
    ('\u{0E01}'..='\u{0E4E}').contains(&c)
}

/// Text rewritten with Thai numerals as ASCII, plus a byte-offset map back
/// into the original (`map[i]` = original offset of normalized offset `i`,
/// with a terminal entry so an end offset maps too). `None` when the input
/// has no Thai numerals — the overwhelmingly common case, no allocation.
struct Normalized {
    text: String,
    map: Vec<usize>,
}

fn normalize_thai_digits(src: &str) -> Option<Normalized> {
    if !src.chars().any(|c| thai_digit(c).is_some()) {
        return None;
    }
    let mut text = String::with_capacity(src.len());
    let mut map = Vec::with_capacity(src.len() + 1);
    for (i, ch) in src.char_indices() {
        match thai_digit(ch) {
            Some(d) => {
                map.push(i);
                text.push(d);
            }
            None => {
                let before = text.len();
                text.push(ch);
                for k in 0..(text.len() - before) {
                    map.push(i + k);
                }
            }
        }
    }
    map.push(src.len());
    Some(Normalized { text, map })
}

/// True when an ASCII digit abuts the match — the run is part of a longer
/// number (bank account, order id), not a phone or an ID.
fn digit_adjacent(text: &str, start: usize, end: usize) -> bool {
    let before = text[..start]
        .chars()
        .next_back()
        .is_some_and(|c| c.is_ascii_digit());
    let after = text[end..]
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_digit());
    before || after
}

/// True when the match starts mid-word (a Thai letter immediately before it).
fn starts_mid_word(text: &str, start: usize) -> bool {
    text[..start]
        .chars()
        .next_back()
        .is_some_and(is_thai_letter)
}

/// A consonant+number run is a plate only with a plate marker just before it
/// or a province right after. Without one, Thai prose is full of them
/// ("ครบ 3 ปี", "ของ 16:9").
fn plate_has_context(text: &str, start: usize, end: usize) -> bool {
    let window: String = text[..start]
        .chars()
        .rev()
        .take(24)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if PLATE_MARKERS.iter().any(|m| window.contains(m)) {
        return true;
    }
    let after = text[end..].trim_start();
    PROVINCES.iter().any(|p| after.starts_with(p))
}

/// Collect a pattern's match ranges so each detector can post-filter them
/// (boundary / context checks) before they become spans.
fn ranges(re: &Regex, text: &str, out: &mut Vec<(usize, usize)>) {
    out.extend(re.find_iter(text).map(|m| (m.start(), m.end())));
}

/// Normalize a value for coreference matching (drop separators, lowercase).
fn normalize(v: &str) -> String {
    v.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect::<String>()
        .to_lowercase()
}

/// Detect all v1 PII spans. `custom` = user CSV terms (exact match, as
/// uploaded). Overlapping spans are resolved by type priority in
/// [`dedupe_overlaps`].
pub fn detect(text: &str, custom: &[String]) -> Vec<Span> {
    match normalize_thai_digits(text) {
        None => detect_inner(text, text, None, custom),
        Some(n) => detect_inner(&n.text, text, Some(&n.map), custom),
    }
}

/// `scan` is what the patterns run against (ASCII-digit form); `orig` is what
/// span offsets and values refer to. They are the same string unless the input
/// carried Thai numerals.
fn detect_inner(scan: &str, orig: &str, map: Option<&[usize]>, custom: &[String]) -> Vec<Span> {
    let to_orig = |off: usize| map.map_or(off, |m| m[off]);
    let mut spans = Vec::new();
    let push = |start: usize, end: usize, kind: PiiType, spans: &mut Vec<Span>| {
        let (s, e) = (to_orig(start), to_orig(end));
        spans.push(Span {
            start: s,
            end: e,
            kind,
            value: orig[s..e].to_string(),
        });
    };

    // ID: regex finds any 13-digit run, checksum filters to real IDs (kills FP).
    // A rejected match can start on a digit just before the ID ("2566 1101…")
    // and swallow its head, so retry one digit later instead of skipping past.
    let mut at = 0;
    while let Some(m) = RE_ID.find_at(scan, at) {
        let digits: String = m.as_str().chars().filter(char::is_ascii_digit).collect();
        if thai_id_valid(&digits) && !digit_adjacent(scan, m.start(), m.end()) {
            push(m.start(), m.end(), PiiType::ThaiId, &mut spans);
            at = m.end();
        } else {
            at = m.start() + 1;
        }
    }
    let mut raw = Vec::new();
    ranges(&RE_PHONE, scan, &mut raw);
    for (s, e) in raw.drain(..) {
        if !digit_adjacent(scan, s, e) {
            push(s, e, PiiType::Phone, &mut spans);
        }
    }
    ranges(&RE_PLATE, scan, &mut raw);
    for (s, e) in raw.drain(..) {
        if !starts_mid_word(scan, s) && plate_has_context(scan, s, e) {
            push(s, e, PiiType::Plate, &mut spans);
        }
    }
    ranges(&RE_NAME, scan, &mut raw);
    for (s, e) in raw.drain(..) {
        if starts_mid_word(scan, s) || NAME_COMPOUNDS.iter().any(|w| scan[s..e].starts_with(w)) {
            continue;
        }
        push(s, e, PiiType::Name, &mut spans);
    }

    // Custom terms match the ORIGINAL text — they're user-supplied literals
    // (account codes, product ids), not digit-normalized patterns.
    for term in custom {
        let t = term.trim();
        if t.is_empty() {
            continue;
        }
        let mut from = 0;
        while let Some(rel) = orig[from..].find(t) {
            let start = from + rel;
            spans.push(Span {
                start,
                end: start + t.len(),
                kind: PiiType::Custom,
                value: t.into(),
            });
            from = start + t.len();
        }
    }

    dedupe_overlaps(spans)
}

/// Overlap-resolution priority: more specific / validated types win. A loose
/// plate match that straddles a phone or ID (Thai prose is full of
/// consonant+number sequences) must yield to it, wherever it starts.
fn prio(k: PiiType) -> u8 {
    match k {
        PiiType::ThaiId => 5, // checksum-validated, strongest signal
        PiiType::Phone => 4,
        PiiType::Custom => 3, // user-declared exact term
        PiiType::Name => 2,
        PiiType::Plate => 1, // loosest pattern, yields on overlap
    }
}

/// Resolve overlaps by priority (ties → longer span). Greedy: place spans
/// high-priority-first, reject any that overlap one already kept. Returned
/// sorted by start so `tokenize` can walk them left-to-right.
fn dedupe_overlaps(mut spans: Vec<Span>) -> Vec<Span> {
    spans.sort_by(|a, b| {
        prio(b.kind)
            .cmp(&prio(a.kind))
            .then((b.end - b.start).cmp(&(a.end - a.start)))
            .then(a.start.cmp(&b.start))
    });
    let mut kept: Vec<Span> = Vec::with_capacity(spans.len());
    for s in spans {
        if kept.iter().any(|k| s.start < k.end && k.start < s.end) {
            continue; // overlaps an already-kept, higher-priority span
        }
        kept.push(s);
    }
    kept.sort_by_key(|s| s.start);
    kept
}

/// Session-scoped pseudonymizer. Holds the placeholder↔value map in memory only
/// (never persisted / never logged) so cloud sees `[NAME_1]`, not the real value.
#[derive(Default)]
pub struct Tokenizer {
    to_real: HashMap<String, String>,         // placeholder → real value
    seen: HashMap<(PiiType, String), String>, // (type, normalized) → placeholder (coreference)
    counters: HashMap<PiiType, usize>,
}

/// Counts only — the maps hold plaintext PII (see [`Span`]'s Debug).
impl fmt::Debug for Tokenizer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tokenizer")
            .field("placeholders", &self.to_real.len())
            .finish()
    }
}

impl Tokenizer {
    pub fn new() -> Self {
        Self::default()
    }

    fn placeholder_for(&mut self, kind: PiiType, value: &str) -> String {
        let key = (kind, normalize(value));
        if let Some(p) = self.seen.get(&key) {
            return p.clone();
        }
        let n = self.counters.entry(kind).or_insert(0);
        *n += 1;
        let p = format!("[{}_{}]", kind.tag(), n);
        self.seen.insert(key, p.clone());
        self.to_real.insert(p.clone(), value.to_string());
        p
    }

    /// Replace `spans` (from `detect`) in `text` with placeholders. Spans must
    /// index into `text`; same value → same placeholder (coreference).
    pub fn tokenize(&mut self, text: &str, spans: &[Span]) -> String {
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0;
        for s in spans {
            if s.start < cursor {
                continue; // defensive: overlapping/unsorted
            }
            out.push_str(&text[cursor..s.start]);
            let p = self.placeholder_for(s.kind, &s.value);
            out.push_str(&p);
            cursor = s.end;
        }
        out.push_str(&text[cursor..]);
        out
    }

    /// Restore placeholders in a (cloud) response back to real values.
    pub fn detokenize(&self, text: &str) -> String {
        RE_PLACEHOLDER
            .replace_all(text, |c: &regex::Captures| {
                let ph = &c[0];
                self.to_real
                    .get(ph)
                    .cloned()
                    .unwrap_or_else(|| ph.to_string())
            })
            .into_owned()
    }
}

// ── Wire boundary (dev-plan/55 step 3) ────────────────────────────────
//
// The invariant everything below preserves: **history stays plaintext,
// the wire carries placeholders.** Masking happens on the way out of the
// process, un-masking on the way back in, and nothing masked is ever
// persisted. That's what makes multi-turn safe — turn 2 re-masks the same
// plaintext and coreference hands back the same `[NAME_1]`, so there's no
// second copy to leak and no way to forget to re-mask.
//
// Thinking blocks are the one exception, deliberately: the model only ever
// saw placeholders, so its reasoning can only contain placeholders. Leaving
// them untouched in BOTH directions keeps them self-consistent and keeps
// Anthropic's signed-thinking blocks byte-identical (rewriting one
// invalidates the signature and the API rejects the turn).

/// Longest placeholder we'll wait for when one straddles a stream chunk
/// (`[PHONE_1234]` = 13). Past this, the `[` was just a bracket.
const MAX_PLACEHOLDER: usize = 24;

/// Wrapper shown around a value that was hidden from the model and put
/// back locally. Display only — never reaches a tool, a file, history, or
/// the wire. Without it the restored value reads as if the model knew it,
/// which is the opposite of what happened.
pub const MARK_OPEN: &str = "🔓« ";
pub const MARK_CLOSE: &str = " »";

/// A restored string in both readings — see [`Masker::restore`].
#[derive(Debug, Default, Clone)]
pub struct Restored {
    /// Undecorated: for tools, files, history, and re-masking.
    pub plain: String,
    /// Marked up for a human reader.
    pub display: String,
}

impl Restored {
    pub fn is_empty(&self) -> bool {
        self.plain.is_empty()
    }
}

/// Process-wide masker, installed from `AppConfig` at worker init. `None`
/// (the default) = feature off and every path below is a no-op.
static ACTIVE: std::sync::OnceLock<std::sync::RwLock<Option<std::sync::Arc<Masker>>>> =
    std::sync::OnceLock::new();

fn slot() -> &'static std::sync::RwLock<Option<std::sync::Arc<Masker>>> {
    ACTIVE.get_or_init(|| std::sync::RwLock::new(None))
}

/// Install or clear the process-wide masker. Called wherever `AppConfig` is
/// pushed into process globals (worker boot + the settings-file watcher), so
/// flipping the Settings toggle takes effect without a restart.
///
/// Refuses to arm under multiuser: one shared worker serves several people
/// and the placeholder map is process-wide, so `[NAME_1]` could resolve to
/// another member's value. Masking is a single-tenant (desktop) feature.
pub fn configure(enabled: bool, custom: Vec<String>) {
    // A test holding the pin owns this global; a worker booting on some other
    // thread must not disarm it out from under an assertion.
    #[cfg(test)]
    if pin_owner::blocks_current_thread() {
        return;
    }
    let arm = enabled && !crate::workdir::is_multiuser();
    let mut g = match slot().write() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    match (&*g, arm) {
        // Already armed with the same dictionary — keep the live map so an
        // unrelated settings change mid-conversation doesn't renumber.
        (Some(m), true) if m.custom == custom => {}
        (_, true) => *g = Some(std::sync::Arc::new(Masker::new(custom))),
        (_, false) => *g = None,
    }
}

/// Test-only exclusive hold on the masking global.
///
/// The masker is a process global, and `configure` is called from every
/// surface's boot path — including the workers `shared_session::spawn_with_roots`
/// starts on detached threads. Those threads outlive the test that created
/// them, so a test asserting on armed state was racing a worker booting with
/// `sensitive_enabled = false` and disarming it mid-assertion. That is how
/// `prompts::masking_briefs_the_model_only_while_armed` came to fail under load
/// while passing alone.
///
/// While a pin is held, `configure` from any *other* thread is ignored — the
/// pinning test still drives it normally from its own thread, so the tests keep
/// exercising the real entry point rather than a stubbed one. The pin also
/// takes the shared test env lock, so pinning tests serialise against each
/// other and against env-var tests without a second lock to order.
///
/// Compiled out entirely outside `cfg(test)`: production has no pin and pays
/// nothing for it.
#[cfg(test)]
pub(crate) struct MaskingPin {
    _env: std::sync::MutexGuard<'static, ()>,
    prev: Option<std::sync::Arc<Masker>>,
}

#[cfg(test)]
mod pin_owner {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Read on every `configure` call, and `configure` runs on every worker
    /// boot — a test spawning fifty of them turned a mutex here into a convoy
    /// that took the suite from sub-second to forty. Unpinned is one relaxed
    /// load and nothing else; the thread-local is only consulted once a pin
    /// actually exists.
    static PINNED: AtomicBool = AtomicBool::new(false);

    thread_local! {
        static IS_OWNER: Cell<bool> = const { Cell::new(false) };
    }

    pub(super) fn claim() {
        IS_OWNER.with(|c| c.set(true));
        PINNED.store(true, Ordering::Release);
    }

    pub(super) fn release() {
        PINNED.store(false, Ordering::Release);
        IS_OWNER.with(|c| c.set(false));
    }

    /// True when someone else pinned the masker — the caller's write is theirs
    /// to lose, not ours to apply.
    pub(super) fn blocks_current_thread() -> bool {
        PINNED.load(Ordering::Acquire) && !IS_OWNER.with(|c| c.get())
    }
}

/// Pin the masking global to this thread for the life of the returned guard.
/// Restores whatever was armed before on drop.
#[cfg(test)]
pub(crate) fn pin_for_test() -> MaskingPin {
    let env = crate::kms::test_env_lock();
    pin_owner::claim();
    MaskingPin {
        _env: env,
        prev: active(),
    }
}

#[cfg(test)]
impl Drop for MaskingPin {
    fn drop(&mut self) {
        let mut g = slot().write().unwrap_or_else(|e| e.into_inner());
        *g = self.prev.take();
        drop(g);
        pin_owner::release();
    }
}

/// The active masker, or `None` when the feature is off.
pub fn active() -> Option<std::sync::Arc<Masker>> {
    match slot().read() {
        Ok(g) => g.clone(),
        Err(e) => e.into_inner().clone(),
    }
}

/// Detector + session tokenizer bound together, shared by every agent in
/// the process so coreference survives across turns and subagents.
pub struct Masker {
    custom: Vec<String>,
    tk: std::sync::Mutex<Tokenizer>,
}

impl fmt::Debug for Masker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Masker")
            .field("custom_terms", &self.custom.len())
            .finish()
    }
}

impl Masker {
    pub fn new(custom: Vec<String>) -> Self {
        Self {
            custom,
            tk: std::sync::Mutex::new(Tokenizer::new()),
        }
    }

    /// Poisoned-lock recovery: the tokenizer can't be left half-updated
    /// (no panics inside its methods), so the data is still sound.
    fn tokenizer(&self) -> std::sync::MutexGuard<'_, Tokenizer> {
        self.tk.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Plaintext → placeholders. Display markers are stripped first, so
    /// decorated text that loops back — a subagent's answer, or a user
    /// pasting a previous reply — masks cleanly instead of shipping
    /// `🔓« [PHONE_1] »`.
    pub fn mask(&self, text: &str) -> String {
        if text.is_empty() {
            return String::new();
        }
        let undecorated;
        let text = if text.contains(MARK_OPEN) {
            undecorated = text.replace(MARK_OPEN, "").replace(MARK_CLOSE, "");
            undecorated.as_str()
        } else {
            text
        };
        let spans = detect(text, &self.custom);
        if spans.is_empty() {
            return text.to_string();
        }
        self.tokenizer().tokenize(text, &spans)
    }

    /// Placeholders → plaintext, undecorated. This is the form that reaches
    /// tools, files and history — a marker in a written file would be a bug.
    pub fn unmask(&self, text: &str) -> String {
        self.restore(text).plain
    }

    /// Both readings of a restored string at once: `plain` for anything the
    /// machine consumes, `display` with each restored value wrapped in
    /// [`MARK_OPEN`]/[`MARK_CLOSE`] so a person can see which parts of the
    /// reply were hidden from the model and put back locally.
    fn restore(&self, text: &str) -> Restored {
        if text.is_empty() {
            return Restored::default();
        }
        let tk = self.tokenizer();
        let mut plain = String::with_capacity(text.len());
        let mut display = String::with_capacity(text.len());
        let mut last = 0;
        let mut restored = 0usize;
        for m in RE_PLACEHOLDER.find_iter(text) {
            plain.push_str(&text[last..m.start()]);
            display.push_str(&text[last..m.start()]);
            match tk.to_real.get(m.as_str()) {
                Some(value) => {
                    plain.push_str(value);
                    display.push_str(MARK_OPEN);
                    display.push_str(value);
                    display.push_str(MARK_CLOSE);
                    restored += 1;
                }
                // Not ours (the model invented it, or the map was reset) —
                // pass it through untouched rather than guessing.
                None => {
                    plain.push_str(m.as_str());
                    display.push_str(m.as_str());
                }
            }
            last = m.end();
        }
        plain.push_str(&text[last..]);
        display.push_str(&text[last..]);
        Restored {
            display: if restored == 0 {
                plain.clone()
            } else {
                display
            },
            plain,
        }
    }

    /// Mask everything in an outbound request that carries user text.
    /// Tool *definitions* are skipped — they're static schema, and
    /// rewriting one would change the contract the model codes against.
    pub fn mask_request(&self, req: &mut crate::providers::StreamRequest) {
        if let Some(system) = &mut req.system {
            *system = self.mask(system);
        }
        for msg in &mut req.messages {
            for block in &mut msg.content {
                self.mask_block(block);
            }
        }
    }

    fn mask_block(&self, block: &mut crate::types::ContentBlock) {
        use crate::types::{ContentBlock, ToolResultBlock, ToolResultContent};
        match block {
            ContentBlock::Text { text } => *text = self.mask(text),
            ContentBlock::ToolUse { input, .. } => self.map_json(input, &|s| self.mask(s)),
            ContentBlock::ToolResult { content, .. } => match content {
                ToolResultContent::Text(t) => *t = self.mask(t),
                ToolResultContent::Blocks(blocks) => {
                    for b in blocks {
                        if let ToolResultBlock::Text { text } = b {
                            *text = self.mask(text);
                        }
                    }
                }
            },
            // See the module note above: reasoning is placeholder-only by
            // construction, and Anthropic signs it.
            ContentBlock::Thinking { .. } => {}
            // A face in a screenshot is real PII that no regex can mask —
            // that's the plan's Mode B (gate) territory, not v1's.
            ContentBlock::Image { .. } => {}
        }
    }

    /// Restore real values inside an assembled tool call's arguments. Done
    /// on the parsed `Value` rather than the streamed JSON fragments, so a
    /// value containing a quote or a backslash can't break the JSON.
    pub fn unmask_json(&self, v: &mut serde_json::Value) {
        self.map_json(v, &|s| self.unmask(s));
    }

    fn map_json(&self, v: &mut serde_json::Value, f: &dyn Fn(&str) -> String) {
        match v {
            serde_json::Value::String(s) => *s = f(s),
            serde_json::Value::Array(items) => {
                for item in items {
                    self.map_json(item, f);
                }
            }
            serde_json::Value::Object(map) => {
                for (_, val) in map.iter_mut() {
                    self.map_json(val, f);
                }
            }
            _ => {}
        }
    }

    /// Un-mask a streamed text chunk. A placeholder can straddle chunk
    /// boundaries (`[NAM` + `E_1]`), so a trailing fragment that might still
    /// become one is held in `pending` until the next chunk or [`flush`].
    ///
    /// [`flush`]: Self::flush
    pub fn feed(&self, pending: &mut String, chunk: &str) -> Restored {
        pending.push_str(chunk);
        let cut = match pending.rfind('[') {
            Some(i) if !pending[i..].contains(']') && pending.len() - i <= MAX_PLACEHOLDER => i,
            _ => pending.len(),
        };
        let ready: String = pending.drain(..cut).collect();
        self.restore(&ready)
    }

    /// Emit whatever [`feed`] is still holding — call at end of stream, or
    /// the last few characters of a reply go missing.
    ///
    /// [`feed`]: Self::feed
    pub fn flush(&self, pending: &mut String) -> Restored {
        let rest = std::mem::take(pending);
        self.restore(&rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_checksum() {
        // constructed valid IDs (checksum-correct); random 13-digit must fail.
        assert!(thai_id_valid("1101700230708"));
        assert!(thai_id_valid("3100600445635"));
        assert!(!thai_id_valid("1101700230700"));
        assert!(!thai_id_valid("1234567890123"));
        assert!(!thai_id_valid("110170023070")); // 12 digits
        assert!(!thai_id_valid("11017002307ab"));
    }

    #[test]
    fn detect_id_needs_checksum() {
        let t = "บัตร 1101700230708 กับ 1234567890123";
        let spans: Vec<_> = detect(t, &[])
            .into_iter()
            .filter(|s| s.kind == PiiType::ThaiId)
            .collect();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].value, "1101700230708");
    }

    /// The printed card groups the digits 1-4-5-2-1; people copy it with
    /// dashes or spaces, and those reached the model unmasked.
    #[test]
    fn detect_id_in_its_printed_grouping() {
        let t = "บัตร 1-1017-00230-70-8 และ 3 1006 00445 63 5 แต่ไม่ใช่ 1-1017-00230-70-0";
        let ids: Vec<_> = detect(t, &[])
            .into_iter()
            .filter(|s| s.kind == PiiType::ThaiId)
            .map(|s| s.value)
            .collect();
        assert_eq!(ids, vec!["1-1017-00230-70-8", "3 1006 00445 63 5"]);
    }

    #[test]
    fn detect_id_after_a_spaced_number() {
        for t in [
            "พ.ศ. 2566 1101700230708",
            "โทร 0812345678 1101700230708",
            "1 1101700230708",
        ] {
            let ids: Vec<_> = detect(t, &[])
                .into_iter()
                .filter(|s| s.kind == PiiType::ThaiId)
                .map(|s| s.value)
                .collect();
            assert_eq!(ids, vec!["1101700230708"], "{t}");
        }
    }

    #[test]
    fn detect_phone_forms() {
        let n = detect("โทร 0812345678 หรือ 08-1234-5678 หรือ +66812345678", &[])
            .into_iter()
            .filter(|s| s.kind == PiiType::Phone)
            .count();
        assert_eq!(n, 3);
    }

    #[test]
    fn detect_plate_and_name_and_custom() {
        let spans = detect(
            "รถทะเบียน กข 1234 นาย สมชาย ใจดี รหัส ACME-42",
            &["ACME-42".into()],
        );
        assert!(spans.iter().any(|s| s.kind == PiiType::Plate));
        assert!(spans.iter().any(|s| s.kind == PiiType::Name));
        assert!(spans
            .iter()
            .any(|s| s.kind == PiiType::Custom && s.value == "ACME-42"));
    }

    #[test]
    fn tokenize_roundtrip_and_coreference() {
        let mut tk = Tokenizer::new();
        let t = "โอนให้ 0812345678 แล้วแจ้ง 0812345678 ด้วย";
        let spans = detect(t, &[]);
        let masked = tk.tokenize(t, &spans);
        // same phone twice → same placeholder (coreference)
        assert!(masked.contains("[PHONE_1]"));
        assert!(!masked.contains("[PHONE_2]"));
        assert!(!masked.contains("0812345678"));
        // round-trip restores the original
        assert_eq!(tk.detokenize(&masked), t);
    }

    #[test]
    fn detokenize_leaves_unknown_placeholders() {
        let tk = Tokenizer::new();
        assert_eq!(tk.detokenize("ค่า [FOO_9] ไม่รู้จัก"), "ค่า [FOO_9] ไม่รู้จัก");
    }

    /// Real Thai prose from `thclaws-tutorial-th/` + `user-manual-th/`,
    /// carrying zero PII. Every line here matched a v1 pattern before the
    /// boundary/context rules landed — 119 hits across five chapters, which
    /// would have shredded every Thai prompt on the way to the cloud. This is
    /// the precision half of the plan's §8 test set; keep adding to it rather
    /// than loosening a detector.
    const NEGATIVE_CORPUS: &[&str] = &[
        "thClaws คือ AI เอเจนต์ที่ทำงานบนเครื่องคุณเป็นหลัก แล้วต่อขึ้นคลาวด์ได้เมื่อต้องการ",
        "ทุกโปรเจกต์คือโฟลเดอร์ที่คุณรัน แชร์ และตั้งให้ทำงานเองได้ มาดูกันว่ามันทำอะไรให้คุณได้บ้าง",
        "คุณไม่ถูกล็อกกับโมเดลเดียว hybrid gateway ส่งงานไปโมเดลในเครื่องแบบส่วนตัว",
        "งานเดียวจบ เหมาะกับสคริปต์ คุณแค่เลือกหน้าตาที่เหมาะกับงาน",
        "ไฟล์อยู่กับคุณ และไม่ถูกใส่ลงในไฟล์โปรเจกต์เด็ดขาด",
        "การ์ดหัวเรื่อง 16:9 พื้นหลังสเลตเข้ม มีโลโก้ thClaws ที่มุมการ์ด",
        "ครบ 3 ปี แล้วนะ",
        "รถ 5 คัน จอดอยู่หน้าบ้าน",
        "ราคา 1200 บาท ลด 10 เปอร์เซ็นต์",
        "ระยะทาง กม 100 จากตัวเมือง",
        "นายกรัฐมนตรีแถลงข่าววันนี้",
        "คุณภาพของสินค้าดีมาก",
        "นายจ้างจ่ายค่าแรงตรงเวลา",
        "นางแบบเดินแฟชั่นโชว์",
        "เลขที่บัญชี 1234508123456789 โอนแล้ว",
        "order id 20250812345678 ครับ",
        "อัปเดตเวอร์ชัน 0.86.1 เมื่อ 3 วันก่อน",
    ];

    #[test]
    fn negative_corpus_has_no_detections() {
        let mut hits = Vec::new();
        for line in NEGATIVE_CORPUS {
            for s in detect(line, &[]) {
                hits.push(format!(
                    "{:?} {:?} in {line:?}",
                    s.kind,
                    &line[s.start..s.end]
                ));
            }
        }
        assert!(
            hits.is_empty(),
            "false positives on PII-free Thai prose:\n{}",
            hits.join("\n")
        );
    }

    /// The recall half: each of these MUST still be caught. A precision fix
    /// that silences one of these traded a leak for a clean corpus.
    #[test]
    fn positive_corpus_still_detected() {
        let cases: &[(&str, PiiType)] = &[
            ("ผู้ป่วยชื่อ นายสมชาย ใจดี อายุ 45 ปี", PiiType::Name),
            ("ติดต่อ น.ส.สมหญิง รักไทย ได้ที่", PiiType::Name),
            ("โทรหาผมที่ 081-234-5678 นะครับ", PiiType::Phone),
            ("บัตรประชาชน 1101700230708 หมดอายุแล้ว", PiiType::ThaiId),
            ("รถทะเบียน กข 1234 จอดขวางอยู่", PiiType::Plate),
            ("ทะเบียน 1กก 9999 เชียงใหม่", PiiType::Plate),
            ("รถ ขข 5678 ภูเก็ต ของใคร", PiiType::Plate),
        ];
        for (text, want) in cases {
            let spans = detect(text, &[]);
            assert!(
                spans.iter().any(|s| s.kind == *want),
                "missed {want:?} in {text:?} — got {spans:?}"
            );
        }
    }

    #[test]
    fn thai_numeral_id_and_phone_are_not_missed() {
        // ๐-๙ used to slip past: `\d` matched them, then the byte-length
        // check in thai_id_valid failed → real ID forwarded un-tokenized.
        let t = "บัตร ๑๑๐๑๗๐๐๒๓๐๗๐๘ โทร ๐๘๑๒๓๔๕๖๗๘";
        let spans = detect(t, &[]);
        let id = spans
            .iter()
            .find(|s| s.kind == PiiType::ThaiId)
            .expect("thai-numeral ID missed");
        assert_eq!(
            id.value, "๑๑๐๑๗๐๐๒๓๐๗๐๘",
            "span must slice the ORIGINAL text"
        );
        assert!(spans.iter().any(|s| s.kind == PiiType::Phone));

        // …and the offsets still index the original, so tokenize/detokenize
        // round-trips byte for byte.
        let mut tk = Tokenizer::new();
        let masked = tk.tokenize(t, &spans);
        assert!(!masked.contains('๑'));
        assert_eq!(tk.detokenize(&masked), t);
    }

    #[test]
    fn phone_inside_a_longer_number_is_not_a_phone() {
        let spans = detect("เลขที่บัญชี 1234508123456789 โอนแล้ว", &[]);
        assert!(
            spans.is_empty(),
            "carved a phone out of an account number: {spans:?}"
        );
    }

    #[test]
    fn plate_without_context_is_not_detected() {
        // Same shape as a plate, but no marker and no province → prose.
        assert!(detect("ครบ 3 ปี แล้วนะ", &[]).is_empty());
        assert!(detect("รถ 5 คัน จอดอยู่", &[]).is_empty());
    }

    #[test]
    fn khun_is_not_a_name_trigger() {
        // `คุณ` is the everyday second-person pronoun; honorific-คุณ names
        // are Layer-2 NER's job (step 4), not a rule detector's.
        assert!(detect("คุณเป็นหลักในงานนี้", &[]).is_empty());
        assert!(detect("ขอบคุณ ครับ", &[]).is_empty());
    }

    // ── Wire boundary (step 3) ────────────────────────────────────────

    fn req(
        system: &str,
        blocks: Vec<crate::types::ContentBlock>,
    ) -> crate::providers::StreamRequest {
        crate::providers::StreamRequest {
            model: "test".into(),
            system: Some(system.to_string()),
            messages: vec![crate::types::Message {
                role: crate::types::Role::User,
                content: blocks,
            }],
            tools: Vec::new(),
            max_tokens: 128,
            thinking_budget: None,
            stream_chunk_timeout_override: None,
        }
    }

    #[test]
    fn mask_request_covers_every_text_carrying_block() {
        use crate::types::{ContentBlock, ToolResultContent};
        let m = Masker::new(Vec::new());
        let mut r = req(
            "ผู้ติดต่อ 0812345678",
            vec![
                ContentBlock::text("บัตร 1101700230708"),
                ContentBlock::ToolUse {
                    id: "1".into(),
                    name: "Write".into(),
                    input: serde_json::json!({"content": "โทร 0812345678", "n": 5}),
                    thought_signature: None,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "1".into(),
                    content: ToolResultContent::Text("อ่านไฟล์ได้ 1101700230708".into()),
                    is_error: false,
                },
                // Signed reasoning must survive byte-for-byte: rewriting it
                // invalidates the signature and the provider rejects the turn.
                ContentBlock::Thinking {
                    content: "ผู้ใช้ให้ [PHONE_1] มา".into(),
                    signature: Some("sig".into()),
                },
            ],
        );
        m.mask_request(&mut r);

        let wire = serde_json::to_string(&r.messages).unwrap() + r.system.as_deref().unwrap();
        assert!(!wire.contains("0812345678"), "phone on the wire: {wire}");
        assert!(!wire.contains("1101700230708"), "id on the wire: {wire}");
        assert!(wire.contains("[PHONE_1]") && wire.contains("[ID_1]"));
        // Non-string JSON values are left alone.
        if let ContentBlock::ToolUse { input, .. } = &r.messages[0].content[1] {
            assert_eq!(input["n"], 5);
        } else {
            panic!("tool_use block moved");
        }
        if let ContentBlock::Thinking { content, .. } = &r.messages[0].content[3] {
            assert_eq!(content, "ผู้ใช้ให้ [PHONE_1] มา", "signed thinking rewritten");
        } else {
            panic!("thinking block moved");
        }
    }

    #[test]
    fn same_value_masks_to_one_placeholder_across_turns() {
        // The multi-turn invariant: history stays plaintext, so turn 2
        // re-masks the same value and must land on the same placeholder —
        // otherwise the model sees two "different" people, and a stale
        // masked copy would be a second thing to leak.
        let m = Masker::new(Vec::new());
        let t1 = m.mask("โทร 0812345678");
        let t2 = m.mask("ยืนยันเบอร์ 081-234-5678 อีกครั้ง");
        assert!(
            t1.contains("[PHONE_1]") && t2.contains("[PHONE_1]"),
            "{t1} / {t2}"
        );
        assert!(
            !t2.contains("[PHONE_2]"),
            "separator variant renumbered: {t2}"
        );
    }

    #[test]
    fn unmask_reassembles_a_placeholder_split_across_chunks() {
        let m = Masker::new(Vec::new());
        let masked = m.mask("โทร 0812345678");
        assert!(masked.contains("[PHONE_1]"));

        let mut pending = String::new();
        let (mut plain, mut display) = (String::new(), String::new());
        // Worst case: one character at a time.
        for ch in "ติดต่อ [PHONE_1] ครับ".chars() {
            let r = m.feed(&mut pending, &ch.to_string());
            plain.push_str(&r.plain);
            display.push_str(&r.display);
        }
        let tail = m.flush(&mut pending);
        plain.push_str(&tail.plain);
        display.push_str(&tail.display);

        assert_eq!(plain, "ติดต่อ 0812345678 ครับ");
        assert_eq!(
            display,
            format!("ติดต่อ {MARK_OPEN}0812345678{MARK_CLOSE} ครับ"),
            "the reader must see which value was put back"
        );
    }

    #[test]
    fn feed_leaves_plain_text_undecorated() {
        // No placeholder → the two readings must be identical, or every
        // ordinary reply would pick up stray markers.
        let m = Masker::new(Vec::new());
        let mut pending = String::new();
        let r = m.feed(&mut pending, "ข้อความธรรมดา ");
        assert_eq!(r.plain, r.display);
        assert!(!r.display.contains('«'));
    }

    #[test]
    fn masking_strips_display_markers_before_going_back_out() {
        // A subagent's answer (or a user pasting a previous reply) carries
        // the wrapper; it must not reach the wire as `🔓« [PHONE_1] »`.
        let m = Masker::new(Vec::new());
        let decorated = format!("เบอร์ {MARK_OPEN}0812345678{MARK_CLOSE} ครับ");
        let masked = m.mask(&decorated);
        assert_eq!(masked, "เบอร์ [PHONE_1] ครับ");
    }

    #[test]
    fn display_markers_never_reach_tool_arguments() {
        let m = Masker::new(Vec::new());
        m.mask("โทร 0812345678");
        let mut v = serde_json::json!({"content": "เบอร์ [PHONE_1]"});
        m.unmask_json(&mut v);
        assert_eq!(v["content"], "เบอร์ 0812345678");
    }

    #[test]
    fn feed_does_not_stall_on_a_bracket_that_never_closes() {
        let m = Masker::new(Vec::new());
        let mut pending = String::new();
        let long = format!("[{}", "x".repeat(MAX_PLACEHOLDER + 5));
        let out = m.feed(&mut pending, &long);
        assert!(!out.is_empty(), "a stray '[' held the whole stream back");
    }

    #[test]
    fn unmask_json_restores_nested_arguments() {
        let m = Masker::new(Vec::new());
        m.mask("โทร 0812345678"); // seed the map
        let mut v = serde_json::json!({
            "path": "note.txt",
            "lines": ["เบอร์ [PHONE_1]", 7],
            "meta": {"who": "[PHONE_1]"}
        });
        m.unmask_json(&mut v);
        assert_eq!(v["lines"][0], "เบอร์ 0812345678");
        assert_eq!(v["lines"][1], 7);
        assert_eq!(v["meta"]["who"], "0812345678");
    }

    #[test]
    fn span_debug_does_not_leak_the_value() {
        let spans = detect("บัตร 1101700230708", &[]);
        let rendered = format!("{:?}", spans);
        assert!(
            !rendered.contains("1101700230708"),
            "Span Debug leaked PII: {rendered}"
        );
        assert!(rendered.contains("ThaiId"));
    }
}
