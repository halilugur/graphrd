//! Full-text search over literals: tokenizer, stemmers, query language and scoring.
//!
//! The postings live in an LSM index owned by the store, keyed by
//! `[key, literal_id, 0, 0]`. A word contributes its token, its English and
//! Turkish stems and every prefix of length >= 3 (for `prefix*`). Each literal
//! also has one `[STATS, literal_id, word_count, 1]` posting, from which BM25
//! takes the corpus size and average length. For typo-tolerant matching, the
//! vocabulary maps each padded trigram of a word to the word:
//! `[gram_key, token_key, 0, 2]`. Hash collisions are harmless: every
//! candidate is verified against the literal's actual text.
//!
//! Query syntax: `word word` (all must match, any inflection), `"exact phrase"`,
//! `prefix*`, `word~` / `word~1` / `word~2` (typo-tolerant), `a OR b`, `-excluded`.
//! A query that finds nothing is retried with its words typo-tolerant (one edit, then two).

/// The shortest prefix that is indexed as a prefix (`prefix*` needs it).
pub const PREFIX_MIN: usize = 3;

/// The posting key of the per-literal statistics postings.
pub const STATS: u32 = u32::MAX;

/// BM25 parameters.
const K1: f64 = 1.2;
const B: f64 = 0.75;

/// Lower-cased alphanumeric tokens.
pub fn tokenize(text: &str) -> Vec<String> {
    spans(text).into_iter().map(|(_, _, t)| t).collect()
}

/// Tokens with their byte ranges in `text`.
fn spans(text: &str) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut start = 0;
    for (i, c) in text.char_indices() {
        if c.is_alphanumeric() {
            if cur.is_empty() {
                start = i;
            }
            // Turkish dotted capital I lower-cases to a plain i (not i + combining dot)
            if c == 'İ' {
                cur.push('i');
            } else {
                cur.extend(c.to_lowercase());
            }
        } else if !cur.is_empty() {
            out.push((start, i, std::mem::take(&mut cur)));
        }
    }
    if !cur.is_empty() {
        out.push((start, text.len(), cur));
    }
    out
}

fn fnv(tag: u8, s: &str) -> u32 {
    let mut h: u64 = 0xcbf29ce484222325 ^ tag as u64;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    // STATS is reserved
    (((h >> 32) ^ h) as u32).min(STATS - 1)
}

/// The posting key of a word.
pub fn token_key(token: &str) -> u32 {
    fnv(b't', token)
}

/// The posting key of a prefix.
pub fn prefix_key(prefix: &str) -> u32 {
    fnv(b'p', prefix)
}

/// The posting key of a stem.
pub fn stem_key(stem: &str) -> u32 {
    fnv(b's', stem)
}

/// The posting key of a trigram.
pub fn gram_key(gram: &str) -> u32 {
    fnv(b'g', gram)
}

/// The language a literal's words are stemmed in, from its language tag.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Lang {
    /// The language a literal's stemmer runs in, from its tag.
    En,
    /// A Turkish-tagged literal.
    Tr,
    /// No (or another) tag: both stemmers apply.
    Any,
}

/// The language of a language tag.
pub fn lang_of(tag: Option<&str>) -> Lang {
    match tag
        .map(|t| t.split('-').next().unwrap_or("").to_ascii_lowercase())
        .as_deref()
    {
        Some("en") => Lang::En,
        Some("tr") => Lang::Tr,
        _ => Lang::Any,
    }
}

/// A token's (English, Turkish) stems, where its language allows them.
fn stems_of(tok: &str, lang: Lang) -> [Option<String>; 2] {
    stems(tok, lang)
}

fn stems(tok: &str, lang: Lang) -> [Option<String>; 2] {
    [
        (lang != Lang::Tr).then(|| stem_en(tok)),
        (lang != Lang::En).then(|| stem_tr(tok)),
    ]
}

/// Character trigrams of a word padded with two `$` on each side.
fn grams(word: &str) -> Vec<String> {
    let chars: Vec<char> = "$$"
        .chars()
        .chain(word.chars())
        .chain("$$".chars())
        .collect();
    let mut out: Vec<String> = chars.windows(3).map(|w| w.iter().collect()).collect();
    out.sort();
    out.dedup();
    out
}

/// The vocabulary entries of a word: `[gram_key, token_key, 0, 2]` per trigram (none for
/// words shorter than 3 characters, which are only matched exactly).
pub fn vocab_entries(tok: &str) -> Vec<[u32; 4]> {
    if tok.chars().count() < 3 {
        return Vec::new();
    }
    let t = token_key(tok);
    grams(tok).iter().map(|g| [gram_key(g), t, 0, 2]).collect()
}

/// Every posting key a literal's text contributes (the statistics posting aside).
pub fn posting_keys(text: &str, lang: Lang) -> Vec<u32> {
    posting_keys_of(&tokenize(text), lang, &mut |t| stem_keys(t))
}

/// The stem posting keys of a word: (English, Turkish).
pub fn stem_keys(tok: &str) -> [u32; 2] {
    [stem_key(&stem_en(tok)), stem_key(&stem_tr(tok))]
}

/// Posting keys of a literal's tokens; `stems` gives a word's `stem_keys` (the store caches them).
pub fn posting_keys_of(
    tokens: &[String],
    lang: Lang,
    stems: &mut dyn FnMut(&str) -> [u32; 2],
) -> Vec<u32> {
    let mut keys = Vec::new();
    for tok in tokens {
        keys.push(token_key(tok));
        // a word is also a prefix of itself, so `data*` finds "data"
        for (i, _) in tok.char_indices().skip(PREFIX_MIN) {
            keys.push(prefix_key(&tok[..i]));
        }
        if tok.chars().count() >= PREFIX_MIN {
            keys.push(prefix_key(tok));
        }
        let [en, tr] = stems(tok);
        if lang != Lang::Tr {
            keys.push(en);
        }
        if lang != Lang::En {
            keys.push(tr);
        }
    }
    keys.sort_unstable();
    keys.dedup();
    keys
}

// ------------------------------------------------------------ stemmers

/// The Porter (1980) stemmer for English.
pub fn stem_en(word: &str) -> String {
    if word.len() <= 2 || !word.bytes().all(|b| b.is_ascii_lowercase()) {
        return word.to_string();
    }
    let mut w: Vec<u8> = word.bytes().collect();
    fn cons(w: &[u8], i: usize) -> bool {
        match w[i] {
            b'a' | b'e' | b'i' | b'o' | b'u' => false,
            b'y' => i == 0 || !cons(w, i - 1),
            _ => true,
        }
    }
    // m: the number of vowel-consonant sequences in w[..n]
    fn m(w: &[u8], n: usize) -> usize {
        let mut i = 0;
        while i < n && cons(w, i) {
            i += 1;
        }
        let mut count = 0;
        loop {
            while i < n && !cons(w, i) {
                i += 1;
            }
            if i >= n {
                return count;
            }
            while i < n && cons(w, i) {
                i += 1;
            }
            count += 1;
        }
    }
    fn has_vowel(w: &[u8], n: usize) -> bool {
        (0..n).any(|i| !cons(w, i))
    }
    fn double_cons(w: &[u8], n: usize) -> bool {
        n >= 2 && w[n - 1] == w[n - 2] && cons(w, n - 1)
    }
    // consonant-vowel-consonant ending, the last not w, x or y
    fn cvc(w: &[u8], n: usize) -> bool {
        n >= 3
            && cons(w, n - 1)
            && !cons(w, n - 2)
            && cons(w, n - 3)
            && !matches!(w[n - 1], b'w' | b'x' | b'y')
    }
    fn ends(w: &[u8], s: &str) -> bool {
        w.ends_with(s.as_bytes())
    }
    // replace suffix `s` by `r` when the stem before it has m > min
    fn rep(w: &mut Vec<u8>, s: &str, r: &str, min: usize) -> bool {
        if !ends(w, s) {
            return false;
        }
        let stem = w.len() - s.len();
        if m(w, stem) > min {
            w.truncate(stem);
            w.extend_from_slice(r.as_bytes());
        }
        true
    }
    // step 1a
    if ends(&w, "sses") || ends(&w, "ies") {
        w.truncate(w.len() - 2);
    } else if !ends(&w, "ss") && ends(&w, "s") {
        w.pop();
    }
    // step 1b
    let mut extra = false;
    if ends(&w, "eed") {
        if m(&w, w.len() - 3) > 0 {
            w.pop();
        }
    } else {
        for s in ["ed", "ing"] {
            if ends(&w, s) && has_vowel(&w, w.len() - s.len()) {
                w.truncate(w.len() - s.len());
                extra = true;
                break;
            }
        }
    }
    if extra {
        let n = w.len();
        if ends(&w, "at") || ends(&w, "bl") || ends(&w, "iz") {
            w.push(b'e');
        } else if double_cons(&w, n) && !matches!(w[n - 1], b'l' | b's' | b'z') {
            w.pop();
        } else if m(&w, n) == 1 && cvc(&w, n) {
            w.push(b'e');
        }
    }
    // step 1c
    if ends(&w, "y") && has_vowel(&w, w.len() - 1) {
        let n = w.len();
        w[n - 1] = b'i';
    }
    // step 2
    for (s, r) in [
        ("ational", "ate"),
        ("tional", "tion"),
        ("enci", "ence"),
        ("anci", "ance"),
        ("izer", "ize"),
        ("abli", "able"),
        ("alli", "al"),
        ("entli", "ent"),
        ("eli", "e"),
        ("ousli", "ous"),
        ("ization", "ize"),
        ("ation", "ate"),
        ("ator", "ate"),
        ("alism", "al"),
        ("iveness", "ive"),
        ("fulness", "ful"),
        ("ousness", "ous"),
        ("aliti", "al"),
        ("iviti", "ive"),
        ("biliti", "ble"),
    ] {
        if rep(&mut w, s, r, 0) {
            break;
        }
    }
    // step 3
    for (s, r) in [
        ("icate", "ic"),
        ("ative", ""),
        ("alize", "al"),
        ("iciti", "ic"),
        ("ical", "ic"),
        ("ful", ""),
        ("ness", ""),
    ] {
        if rep(&mut w, s, r, 0) {
            break;
        }
    }
    // step 4
    for s in [
        "al", "ance", "ence", "er", "ic", "able", "ible", "ant", "ement", "ment", "ent", "ion",
        "ou", "ism", "ate", "iti", "ous", "ive", "ize",
    ] {
        if ends(&w, s) {
            let stem = w.len() - s.len();
            let ok = s != "ion" || (stem > 0 && matches!(w[stem - 1], b's' | b't'));
            if ok && m(&w, stem) > 1 {
                w.truncate(stem);
            }
            break;
        }
    }
    // step 5
    if ends(&w, "e") {
        let stem = w.len() - 1;
        let mm = m(&w, stem);
        if mm > 1 || (mm == 1 && !cvc(&w, stem)) {
            w.pop();
        }
    }
    let n = w.len();
    if m(&w, n) > 1 && double_cons(&w, n) && w[n - 1] == b'l' {
        w.pop();
    }
    String::from_utf8(w).unwrap_or_else(|_| word.to_string())
}

/// Turkish noun suffixes (case, possessive, plural, copula), longest first. Forms with the
/// buffer consonant n (`-nı`, `-nda`, `-nın`, ...) are left out: they read the same as a root
/// ending in n plus a suffix (`veritaban-ı`, `zaman-ın`), and stripping the vowel suffix and
/// then the possessive (`kitab-ı-n-ı` → `kitabın` → `kitab`) reaches the same stem.
const TR_SUFFIXES: &[&str] = &[
    "ımız", "imiz", "umuz", "ümüz", "ınız", "iniz", "unuz", "ünüz", "daki", "deki", "taki", "teki",
    "ları", "leri", "dan", "den", "tan", "ten", "yla", "yle", "lar", "ler", "dır", "dir", "dur",
    "dür", "tır", "tir", "tur", "tür", "da", "de", "ta", "te", "ın", "in", "un", "ün", "la", "le",
    "ya", "ye", "yı", "yi", "yu", "yü", "ca", "ce", "ça", "çe", "sı", "si", "su", "sü", "ım", "im",
    "um", "üm", "ki", "a", "e", "ı", "i", "u", "ü",
];

/// A light Turkish stemmer: strips up to three inflectional noun suffixes
/// (`kitaplarımızdan` → `kitap`) and undoes final consonant softening (`kitabı` → `kitap`).
pub fn stem_tr(word: &str) -> String {
    let is_vowel = |c: char| "aeıioöuü".contains(c);
    let mut w: Vec<char> = word.chars().collect();
    if !w.iter().all(|c| c.is_alphabetic()) {
        return word.to_string();
    }
    let mut stripped = false;
    for _ in 0..3 {
        let mut cut = None;
        for s in TR_SUFFIXES {
            let n = s.chars().count();
            let rest = w.len().saturating_sub(n);
            // the stem keeps a vowel and 2 letters (3 when only a vowel is removed)
            if w.len() > n
                && rest >= if n == 1 { 3 } else { 2 }
                && w[rest..].iter().copied().eq(s.chars())
                && w[..rest].iter().any(|&c| is_vowel(c))
            {
                cut = Some(rest);
                break;
            }
        }
        match cut {
            Some(rest) => {
                w.truncate(rest);
                stripped = true;
            }
            None => break,
        }
    }
    if let (true, Some(last)) = (stripped, w.last_mut()) {
        *last = match *last {
            'b' => 'p',
            'c' => 'ç',
            'ğ' => 'k',
            c => c,
        };
    }
    w.into_iter().collect()
}

/// Optimal string alignment distance (Levenshtein plus adjacent transpositions), or None
/// when it exceeds `max`.
pub fn edit_distance(a: &str, b: &str, max: usize) -> Option<usize> {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > max {
        return None;
    }
    let mut prev2 = vec![0usize; b.len() + 1];
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        let mut best = cur[0];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                cur[j] = cur[j].min(prev2[j - 2] + 1);
            }
            best = best.min(cur[j]);
        }
        if best > max {
            return None;
        }
        prev2 = std::mem::replace(&mut prev, cur);
    }
    (prev[b.len()] <= max).then_some(prev[b.len()])
}

/// The edit distance allowed for a word by `word~`: 1 up to 5 letters, then 2.
fn auto_distance(word: &str) -> usize {
    if word.chars().count() <= 5 { 1 } else { 2 }
}

// ------------------------------------------------------------ queries

#[derive(Debug, Clone, PartialEq)]
/// One thing a full-text clause can ask for.
pub enum Clause {
    /// A word and its (English, Turkish) stems: matches any inflection.
    Word(String, [String; 2]),
    /// A prefix the word must start with.
    Prefix(String),
    /// The words, one after another.
    Phrase(Vec<String>),
    /// A word within an edit distance.
    Fuzzy(String, usize),
}

/// How to collect a clause's candidates from the postings.
pub enum Plan {
    /// Literals in every posting list.
    All(Vec<u32>),
    /// Literals in any posting list.
    Any(Vec<u32>),
    /// Literals holding a word that has at least `n` of these vocabulary trigrams.
    AtLeast(Vec<u32>, usize),
}

fn word(w: &str) -> Clause {
    Clause::Word(w.to_string(), [stem_en(w), stem_tr(w)])
}

#[derive(Debug, Default, Clone)]
/// A parsed full-text query.
pub struct FtsQuery {
    /// Each group must match (AND); a group matches if any clause in it does (OR).
    pub groups: Vec<Vec<Clause>>,
    /// Clauses no result may match.
    pub excluded: Vec<Clause>,
}

/// Parse the query language: words, `"exact phrases"`, `prefix*`, `a OR b`,
/// `-excluded`, `word~`/`~1`/`~2`.
pub fn parse_query(q: &str) -> FtsQuery {
    let mut out = FtsQuery::default();
    let mut pending_or = false;
    let mut chars = q.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let Some(&c) = chars.peek() else { break };
        let negate = c == '-';
        if negate {
            chars.next();
        }
        let clause = if chars.peek() == Some(&'"') {
            chars.next();
            let mut phrase = String::new();
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                phrase.push(c);
            }
            let toks = tokenize(&phrase);
            if toks.is_empty() {
                continue;
            }
            Clause::Phrase(toks)
        } else {
            let mut w = String::new();
            while chars.peek().is_some_and(|c| !c.is_whitespace()) {
                w.push(chars.next().unwrap());
            }
            if w == "OR" {
                pending_or = true;
                continue;
            }
            // word~, word~1, word~2
            let fuzzy = w.rfind('~').and_then(|i| match &w[i + 1..] {
                "" => Some((i, None)),
                "0" | "1" | "2" => Some((i, w[i + 1..].parse::<usize>().ok())),
                _ => None,
            });
            let (body, fuzzy) = match fuzzy {
                Some((i, d)) => (&w[..i], Some(d)),
                None => (w.as_str(), None),
            };
            let prefix = body.ends_with('*');
            let toks = tokenize(body.trim_end_matches('*'));
            match (toks.len(), prefix, fuzzy) {
                (0, _, _) => continue,
                (1, _, Some(d)) => Clause::Fuzzy(
                    toks[0].clone(),
                    d.unwrap_or_else(|| auto_distance(&toks[0])),
                ),
                (1, true, _) => Clause::Prefix(toks[0].clone()),
                (1, false, _) => word(&toks[0]),
                _ => Clause::Phrase(toks),
            }
        };
        if negate {
            out.excluded.push(clause);
        } else if pending_or && !out.groups.is_empty() {
            out.groups.last_mut().unwrap().push(clause);
        } else {
            out.groups.push(vec![clause]);
        }
        pending_or = false;
    }
    out
}

/// A literal's text prepared for matching: its tokens, and their stems once needed.
pub struct Doc {
    tokens: Vec<String>,
    lang: Lang,
    stems: std::cell::RefCell<Vec<Option<[Option<String>; 2]>>>,
}

impl Doc {
    /// A literal indexed for search, with its stems.
    pub fn new(text: &str, lang: Lang) -> Doc {
        let tokens = tokenize(text);
        let stems = std::cell::RefCell::new(vec![None; tokens.len()]);
        Doc {
            tokens,
            lang,
            stems,
        }
    }

    /// Does token `i` have the stem `en` (English) or `tr` (Turkish)?
    fn stem_is(&self, i: usize, en: &str, tr: &str) -> bool {
        let t = &self.tokens[i];
        // a stem keeps the word's beginning (only its last letter may differ), so most tokens
        // are ruled out without stemming them
        fn head(s: &str) -> &str {
            s.char_indices().last().map_or(s, |(j, _)| &s[..j])
        }
        if !t.starts_with(head(en)) && !t.starts_with(head(tr)) {
            return false;
        }
        let mut stems = self.stems.borrow_mut();
        let [ten, ttr] = stems[i].get_or_insert_with(|| stems_of(t, self.lang));
        ten.as_deref() == Some(en) || ttr.as_deref() == Some(tr)
    }

    /// The number of tokens (the document's length for BM25).
    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    /// Whether there is nothing to match.
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }
}

impl Clause {
    /// Where this clause's candidates come from.
    pub fn plan(&self) -> Plan {
        match self {
            Clause::Word(_, [en, tr]) => {
                let mut keys = vec![stem_key(en), stem_key(tr)];
                keys.dedup();
                Plan::Any(keys)
            }
            Clause::Prefix(p) if p.chars().count() >= PREFIX_MIN => Plan::All(vec![prefix_key(p)]),
            Clause::Prefix(p) => Plan::All(vec![token_key(p)]), // too short to have prefix postings
            Clause::Phrase(t) => Plan::All(t.iter().map(|w| token_key(w)).collect()),
            Clause::Fuzzy(w, d) if w.chars().count() >= 3 => {
                // each edit changes at most 4 of the padded trigrams
                let keys: Vec<u32> = grams(w).iter().map(|g| gram_key(g)).collect();
                let need = keys.len().saturating_sub(4 * d).max(1);
                Plan::AtLeast(keys, need)
            }
            Clause::Fuzzy(w, _) => Plan::All(vec![token_key(w)]),
        }
    }

    /// The weight of token `i` of `doc` for this clause (single-word clauses): 1 for a match,
    /// less for a typo-tolerant one, None when it does not match.
    fn token_weight(&self, doc: &Doc, i: usize) -> Option<f64> {
        let t = &doc.tokens[i];
        match self {
            Clause::Word(w, [en, tr]) => (t == w || doc.stem_is(i, en, tr)).then_some(1.0),
            Clause::Prefix(p) => t.starts_with(p.as_str()).then_some(1.0),
            Clause::Fuzzy(w, d) => edit_distance(t, w, *d).map(|e| 1.0 / (1.0 + e as f64)),
            Clause::Phrase(_) => None,
        }
    }

    /// Term frequency of this clause in `doc` (0 when it does not match).
    fn tf(&self, doc: &Doc) -> f64 {
        match self {
            Clause::Phrase(ph) => {
                (doc.tokens
                    .windows(ph.len())
                    .filter(|w| *w == ph.as_slice())
                    .count()
                    * ph.len()) as f64
            }
            _ => (0..doc.tokens.len())
                .filter_map(|i| self.token_weight(doc, i))
                .sum(),
        }
    }

    /// Tokens of `doc` this clause matches (for highlighting).
    fn mark(&self, doc: &Doc, marked: &mut [bool]) {
        match self {
            Clause::Phrase(ph) => {
                for (i, w) in doc.tokens.windows(ph.len()).enumerate() {
                    if w == ph.as_slice() {
                        marked[i..i + ph.len()].iter_mut().for_each(|m| *m = true);
                    }
                }
            }
            _ => {
                for (i, m) in marked.iter_mut().enumerate() {
                    if self.token_weight(doc, i).is_some() {
                        *m = true;
                    }
                }
            }
        }
    }
}

/// Corpus statistics for BM25.
#[derive(Debug, Clone, Copy)]
pub struct Corpus {
    /// How many literals the index holds.
    pub docs: f64,
    /// The average literal length.
    pub avg_len: f64,
}

impl FtsQuery {
    /// Whether the query matches nothing by construction.
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// The positive clauses in order (the order of `dfs` in `score`).
    pub fn clauses(&self) -> impl Iterator<Item = &Clause> {
        self.groups.iter().flatten()
    }

    /// The query with its words of 3+ letters typo-tolerant, up to `max` edits each (None
    /// when that changes nothing).
    pub fn fuzzy(&self, max: usize) -> Option<FtsQuery> {
        let mut changed = false;
        let groups = self
            .groups
            .iter()
            .map(|g| {
                g.iter()
                    .map(|c| match c {
                        Clause::Word(w, _) if w.chars().count() >= 3 => {
                            changed = true;
                            Clause::Fuzzy(w.clone(), auto_distance(w).min(max))
                        }
                        c => c.clone(),
                    })
                    .collect()
            })
            .collect();
        changed.then(|| FtsQuery {
            groups,
            excluded: self.excluded.clone(),
        })
    }

    /// The query, then the forms tried when it matches nothing: one edit per word, then the
    /// full `word~` allowance (one edit catches most typos and finds far fewer candidates).
    pub fn forms(self) -> Vec<FtsQuery> {
        let mut out = Vec::new();
        for f in [self.fuzzy(1), self.fuzzy(2)].into_iter().flatten() {
            if !out.iter().any(|o: &FtsQuery| o.groups == f.groups) {
                out.push(f);
            }
        }
        out.insert(0, self);
        out
    }

    /// The BM25 relevance of `doc`, or None if it does not match. `dfs` holds each positive
    /// clause's document frequency; without corpus statistics every word counts the same.
    pub fn score(&self, doc: &Doc, corpus: Option<Corpus>, dfs: &[f64]) -> Option<f64> {
        if doc.is_empty() || self.excluded.iter().any(|c| c.tf(doc) > 0.0) {
            return None;
        }
        let len = doc.len() as f64;
        let corpus = corpus.unwrap_or(Corpus {
            docs: 1.0,
            avg_len: len,
        });
        let mut score = 0.0;
        let mut i = 0;
        for group in &self.groups {
            let mut any = false;
            for c in group {
                let tf = c.tf(doc);
                let df = dfs.get(i).copied().unwrap_or(1.0).min(corpus.docs);
                i += 1;
                if tf > 0.0 {
                    any = true;
                    let idf = (1.0 + (corpus.docs - df + 0.5) / (df + 0.5)).ln();
                    score += idf * tf * (K1 + 1.0)
                        / (tf + K1 * (1.0 - B + B * len / corpus.avg_len.max(1.0)));
                }
            }
            if !any {
                return None;
            }
        }
        Some(score)
    }

    /// `text` with the words the query matches wrapped in `pre` and `post`.
    pub fn highlight(&self, text: &str, lang: Lang, pre: &str, post: &str) -> String {
        let doc = Doc::new(text, lang);
        let mut marked = vec![false; doc.len()];
        for c in self.clauses() {
            c.mark(&doc, &mut marked);
        }
        let mut out = String::new();
        let mut at = 0;
        for ((start, end, _), m) in spans(text).into_iter().zip(&marked) {
            if *m {
                out.push_str(&text[at..start]);
                out.push_str(pre);
                out.push_str(&text[start..end]);
                out.push_str(post);
                at = end;
            }
        }
        out.push_str(&text[at..]);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(q: &str, text: &str) -> bool {
        parse_query(q)
            .score(&Doc::new(text, Lang::Any), None, &[])
            .is_some()
    }

    #[test]
    fn tokenizer_and_queries() {
        assert_eq!(
            tokenize("Hello, Wörld! 42x İstanbul"),
            vec!["hello", "wörld", "42x", "istanbul"]
        );
        let q = parse_query(r#"graph "linked data" data* -sql"#);
        assert_eq!(q.groups.len(), 3);
        assert_eq!(q.excluded, vec![word("sql")]);
        assert!(matches(
            r#"graph "linked data" data* -sql"#,
            "Graph databases store linked data and dataset metadata"
        ));
        assert!(!matches(
            r#"graph "linked data" data* -sql"#,
            "Graph databases store linked data, not SQL"
        ));
        assert!(!matches(
            r#"graph "linked data" data* -sql"#,
            "linked graph"
        ));
        assert!(
            matches("cat OR dog", "my dog")
                && matches("cat OR dog", "my cat")
                && !matches("cat OR dog", "my fish")
        );
        assert!(posting_keys("database", Lang::Any).contains(&prefix_key("data")));
    }

    #[test]
    fn english_stems() {
        for (w, s) in [
            ("caresses", "caress"),
            ("ponies", "poni"),
            ("cats", "cat"),
            ("feed", "feed"),
            ("agreed", "agre"),
            ("plastered", "plaster"),
            ("motoring", "motor"),
            ("hopping", "hop"),
            ("filing", "file"),
            ("happy", "happi"),
            ("relational", "relat"),
            ("conditional", "condit"),
            ("generalization", "gener"),
            ("hopefulness", "hope"),
            ("electrical", "electr"),
            ("adjustment", "adjust"),
            ("adoption", "adopt"),
            ("controll", "control"),
            ("running", "run"),
            ("databases", "databas"),
        ] {
            assert_eq!(stem_en(w), s, "{w}");
        }
        assert!(matches("running databases", "She runs a database"));
        assert!(!matches("running", "a runner"));
    }

    #[test]
    fn turkish_stems() {
        for w in [
            "kitap",
            "kitaplar",
            "kitapların",
            "kitaplarda",
            "kitaplarımızdan",
            "kitabı",
            "kitapta",
        ] {
            assert_eq!(stem_tr(w), "kitap", "{w}");
        }
        for w in ["ev", "evler", "evlerde", "evden", "evimiz"] {
            assert_eq!(stem_tr(w), "ev", "{w}");
        }
        assert_eq!(stem_tr("ağaçlar"), "ağaç");
        assert_eq!(stem_tr("ağacı"), "ağaç");
        assert_eq!(stem_tr("çocuğu"), "çocuk");
        for (a, b) in [
            ("veritabanı", "veritabanları"),
            ("kitabını", "kitap"),
            ("evinde", "ev"),
            ("zamanın", "zaman"),
            ("kitaplarında", "kitap"),
        ] {
            assert_eq!(stem_tr(a), stem_tr(b), "{a} {b}");
        }
        assert!(matches("kitaplar", "Bu kitabı okudum"));
        assert!(matches("İSTANBUL", "İstanbul'da"));
        // an English-tagged literal is not stemmed as Turkish
        let q = parse_query("evler");
        assert!(q.score(&Doc::new("ev", Lang::Tr), None, &[]).is_some());
        assert!(q.score(&Doc::new("ev", Lang::En), None, &[]).is_none());
    }

    #[test]
    fn typos_bm25_and_highlights() {
        assert_eq!(edit_distance("house", "hosue", 2), Some(1));
        assert_eq!(edit_distance("kitten", "sitting", 3), Some(3));
        assert_eq!(edit_distance("kitten", "sitting", 2), None);
        assert!(matches("databse~", "graph database"));
        assert!(!matches("databse", "graph database"));
        assert!(!matches("dtbs~1", "graph database"));
        let fuzzy = parse_query("grpah databse").fuzzy(2).unwrap();
        assert!(
            fuzzy
                .score(&Doc::new("A graph database", Lang::Any), None, &[1.0, 1.0])
                .is_some()
        );
        // every typo-tolerant match shares enough trigrams with the word
        if let Plan::AtLeast(keys, need) = Clause::Fuzzy("hosue".into(), 1).plan() {
            let vocab: std::collections::HashSet<u32> =
                vocab_entries("house").into_iter().map(|e| e[0]).collect();
            assert!(keys.iter().filter(|k| vocab.contains(k)).count() >= need);
        } else {
            panic!("a fuzzy word is planned by trigrams");
        }
        // BM25: a rare word outweighs a common one, a short literal a long one
        let q = parse_query("graph rare");
        let c = Some(Corpus {
            docs: 1000.0,
            avg_len: 5.0,
        });
        let a = q
            .score(&Doc::new("graph graph rare", Lang::Any), c, &[500.0, 2.0])
            .unwrap();
        let b = q
            .score(
                &Doc::new("graph rare graph rare", Lang::Any),
                c,
                &[500.0, 2.0],
            )
            .unwrap();
        assert!(b > a);
        let short = parse_query("graph")
            .score(&Doc::new("graph store", Lang::Any), c, &[10.0])
            .unwrap();
        let long = parse_query("graph")
            .score(
                &Doc::new("graph store with many more words in it", Lang::Any),
                c,
                &[10.0],
            )
            .unwrap();
        assert!(short > long);
        assert_eq!(
            parse_query("database \"linked data\"").highlight(
                "Linked data in a Database!",
                Lang::Any,
                "<b>",
                "</b>"
            ),
            "<b>Linked</b> <b>data</b> in a <b>Database</b>!"
        );
        assert_eq!(
            parse_query("kitap").highlight("Kitapları okudu", Lang::Tr, "[", "]"),
            "[Kitapları] okudu"
        );
    }
}
