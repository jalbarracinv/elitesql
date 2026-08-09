//! Text collation for ORDER BY.
//!
//! Byte order is the wrong answer for human-readable text: `ñ` and every
//! accented vowel encode above `z` in UTF-8, and every uppercase ASCII letter
//! encodes below every lowercase one, so `ORDER BY name` produced neither
//! alphabetical nor case-insensitive order.
//!
//! This is a three-level comparison in the shape of the Unicode Collation
//! Algorithm, over a hand-written table for the Latin scripts rather than the
//! full DUCET, which would mean a multi-megabyte dependency in a core that
//! deliberately has five small ones:
//!
//! 1. **base letters** — accents removed and case folded, so `Ávila` sorts
//!    among the a's rather than after `z`;
//! 2. **diacritics** — only consulted when the base letters tie, so `animo`
//!    and `ánimo` land next to each other instead of at opposite ends;
//! 3. **case** — lowercase before uppercase, only when both earlier levels tie.
//!
//! `ñ` is not a diacritic here. Spanish, Galician and Basque give it its own
//! place in the alphabet, so it carries a primary weight of its own between `n`
//! and `o`: `nube` sorts before `ñu`. That is a deliberate departure from the
//! language-neutral Unicode default, where `ñ` is `n` plus a tilde and `ñu`
//! would come first.
//!
//! Scope and known limits, since a collation that quietly gets a language wrong
//! is worse than one that says so:
//!
//! - Correct for Spanish, Catalan, French, Portuguese, Italian, English and
//!   German dictionary order.
//! - **Wrong** where a language gives some *other* letter its own place in the
//!   alphabet: Swedish/Danish/Norwegian sort `å ä ö` *after* `z`, Czech treats
//!   `ch` as one letter, Polish separates `ł`, Turkish separates dotless `ı`.
//!   Each needs its own tailoring, which this does not have; the primary
//!   weights leave gaps precisely so one can be added the way `ñ` was.
//! - Ligatures and `ß` compare as a single base letter rather than expanding
//!   (`ß` as `s`, not `ss`; `æ` as `a`, not `ae`).
//!
//! Text outside the Latin ranges compares by code point at the primary level,
//! which is stable and total, just not linguistic.
//!
//! Equality is deliberately **not** collated: `find_eq`, unique indexes and the
//! secondary index all key on exact bytes, so making `=` accent-insensitive
//! here would let the SQL layer and the index disagree about the same row.

use std::cmp::Ordering;

/// How text keys are compared while sorting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Collation {
    /// Base letter, then diacritic, then case. The default for ORDER BY.
    #[default]
    Unicode,
    /// Raw UTF-8 byte order.
    Binary,
}

impl Collation {
    pub fn parse(name: &str) -> Option<Collation> {
        match name.to_ascii_lowercase().as_str() {
            "unicode" => Some(Collation::Unicode),
            "binary" => Some(Collation::Binary),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Collation::Unicode => "unicode",
            Collation::Binary => "binary",
        }
    }

    pub fn compare(&self, a: &str, b: &str) -> Ordering {
        match self {
            Collation::Unicode => compare(a, b),
            Collation::Binary => a.cmp(b),
        }
    }
}

/// Three-level comparison. Each level walks both strings independently, so no
/// sort key is ever allocated.
pub fn compare(a: &str, b: &str) -> Ordering {
    a.chars()
        .map(primary)
        .cmp(b.chars().map(primary))
        .then_with(|| a.chars().map(secondary).cmp(b.chars().map(secondary)))
        .then_with(|| a.chars().map(tertiary).cmp(b.chars().map(tertiary)))
}

/// Primary weight: the letter this character is a variant of, case folded.
///
/// Weights are the code point shifted left, which leaves gaps between adjacent
/// letters so a letter that a language treats as its own can be slotted between
/// two others rather than folded onto one of them. `ñ` uses that gap.
fn primary(c: char) -> u32 {
    base(c).0
}

const fn letter(c: char) -> u32 {
    (c as u32) << 2
}

/// A letter of its own, ordered immediately after `after`.
const fn after(c: char) -> u32 {
    letter(c) + 1
}

/// Which accent it carries, if any. Ranked, not merely present/absent, so
/// `á` and `à` keep a stable order relative to each other.
fn secondary(c: char) -> u8 {
    base(c).1
}

/// Lowercase sorts before uppercase.
fn tertiary(c: char) -> u8 {
    u8::from(c.is_uppercase())
}

/// Diacritic ranks. Ordered by how they appear in DUCET's secondary weights,
/// which keeps `a á à â ã ä å` in the order readers expect.
const NONE: u8 = 0;
const ACUTE: u8 = 1;
const GRAVE: u8 = 2;
const CIRCUMFLEX: u8 = 3;
const TILDE: u8 = 4;
const DIAERESIS: u8 = 5;
const RING: u8 = 6;
const CEDILLA: u8 = 7;
const STROKE: u8 = 8;
const CARON: u8 = 9;
const BREVE: u8 = 10;
const MACRON: u8 = 11;
const OGONEK: u8 = 12;
const DOT: u8 = 13;
/// Ligatures and `ß`, which really expand to two letters; folding them onto the
/// first one keeps them adjacent to it instead of scattering them after `z`.
const LIGATURE: u8 = 20;

/// Maps a character to its primary weight and diacritic rank. Characters with
/// no Latin base — punctuation, digits, and every non-Latin script — weigh
/// their own code point, so they compare deterministically by code point.
fn base(c: char) -> (u32, u8) {
    // ASCII is the common case and needs no table.
    if c.is_ascii() {
        return (letter(c.to_ascii_lowercase()), NONE);
    }
    match c {
        // Latin-1 Supplement.
        'À' | 'à' => (letter('a'), GRAVE),
        'Á' | 'á' => (letter('a'), ACUTE),
        'Â' | 'â' => (letter('a'), CIRCUMFLEX),
        'Ã' | 'ã' => (letter('a'), TILDE),
        'Ä' | 'ä' => (letter('a'), DIAERESIS),
        'Å' | 'å' => (letter('a'), RING),
        'Æ' | 'æ' => (letter('a'), LIGATURE),
        'Ç' | 'ç' => (letter('c'), CEDILLA),
        'È' | 'è' => (letter('e'), GRAVE),
        'É' | 'é' => (letter('e'), ACUTE),
        'Ê' | 'ê' => (letter('e'), CIRCUMFLEX),
        'Ë' | 'ë' => (letter('e'), DIAERESIS),
        'Ì' | 'ì' => (letter('i'), GRAVE),
        'Í' | 'í' => (letter('i'), ACUTE),
        'Î' | 'î' => (letter('i'), CIRCUMFLEX),
        'Ï' | 'ï' => (letter('i'), DIAERESIS),
        // Spanish, Galician and Basque give `ñ` its own place in the
        // alphabet, so it takes the gap after `n` rather than folding onto it:
        // `nube` sorts before `ñu`, as a Spanish reader expects.
        'Ñ' | 'ñ' => (after('n'), NONE),
        'Ò' | 'ò' => (letter('o'), GRAVE),
        'Ó' | 'ó' => (letter('o'), ACUTE),
        'Ô' | 'ô' => (letter('o'), CIRCUMFLEX),
        'Õ' | 'õ' => (letter('o'), TILDE),
        'Ö' | 'ö' => (letter('o'), DIAERESIS),
        'Ø' | 'ø' => (letter('o'), STROKE),
        'Ù' | 'ù' => (letter('u'), GRAVE),
        'Ú' | 'ú' => (letter('u'), ACUTE),
        'Û' | 'û' => (letter('u'), CIRCUMFLEX),
        'Ü' | 'ü' => (letter('u'), DIAERESIS),
        'Ý' | 'ý' | 'ÿ' => (letter('y'), ACUTE),
        'ß' => (letter('s'), LIGATURE),
        'Ð' | 'ð' => (letter('d'), STROKE),

        // Latin Extended-A, the letters that actually turn up in European text.
        'Ā' | 'ā' => (letter('a'), MACRON),
        'Ă' | 'ă' => (letter('a'), BREVE),
        'Ą' | 'ą' => (letter('a'), OGONEK),
        'Ć' | 'ć' => (letter('c'), ACUTE),
        'Ĉ' | 'ĉ' => (letter('c'), CIRCUMFLEX),
        'Ċ' | 'ċ' => (letter('c'), DOT),
        'Č' | 'č' => (letter('c'), CARON),
        'Ď' | 'ď' => (letter('d'), CARON),
        'Đ' | 'đ' => (letter('d'), STROKE),
        'Ē' | 'ē' => (letter('e'), MACRON),
        'Ĕ' | 'ĕ' => (letter('e'), BREVE),
        'Ė' | 'ė' => (letter('e'), DOT),
        'Ę' | 'ę' => (letter('e'), OGONEK),
        'Ě' | 'ě' => (letter('e'), CARON),
        'Ĝ' | 'ĝ' => (letter('g'), CIRCUMFLEX),
        'Ğ' | 'ğ' => (letter('g'), BREVE),
        'Ġ' | 'ġ' => (letter('g'), DOT),
        'Ģ' | 'ģ' => (letter('g'), CEDILLA),
        'Ĥ' | 'ĥ' => (letter('h'), CIRCUMFLEX),
        'Ĩ' | 'ĩ' => (letter('i'), TILDE),
        'Ī' | 'ī' => (letter('i'), MACRON),
        'Ĭ' | 'ĭ' => (letter('i'), BREVE),
        'Į' | 'į' => (letter('i'), OGONEK),
        'İ' => (letter('i'), DOT),
        'ı' => (letter('i'), NONE),
        'Ĵ' | 'ĵ' => (letter('j'), CIRCUMFLEX),
        'Ķ' | 'ķ' => (letter('k'), CEDILLA),
        'Ĺ' | 'ĺ' => (letter('l'), ACUTE),
        'Ļ' | 'ļ' => (letter('l'), CEDILLA),
        'Ľ' | 'ľ' => (letter('l'), CARON),
        'Ł' | 'ł' => (letter('l'), STROKE),
        'Ń' | 'ń' => (letter('n'), ACUTE),
        'Ņ' | 'ņ' => (letter('n'), CEDILLA),
        'Ň' | 'ň' => (letter('n'), CARON),
        'Ō' | 'ō' => (letter('o'), MACRON),
        'Ŏ' | 'ŏ' => (letter('o'), BREVE),
        'Ő' | 'ő' => (letter('o'), DIAERESIS),
        'Œ' | 'œ' => (letter('o'), LIGATURE),
        'Ŕ' | 'ŕ' => (letter('r'), ACUTE),
        'Ŗ' | 'ŗ' => (letter('r'), CEDILLA),
        'Ř' | 'ř' => (letter('r'), CARON),
        'Ś' | 'ś' => (letter('s'), ACUTE),
        'Ŝ' | 'ŝ' => (letter('s'), CIRCUMFLEX),
        'Ş' | 'ş' => (letter('s'), CEDILLA),
        'Š' | 'š' => (letter('s'), CARON),
        'Ţ' | 'ţ' => (letter('t'), CEDILLA),
        'Ť' | 'ť' => (letter('t'), CARON),
        'Ŧ' | 'ŧ' => (letter('t'), STROKE),
        'Ũ' | 'ũ' => (letter('u'), TILDE),
        'Ū' | 'ū' => (letter('u'), MACRON),
        'Ŭ' | 'ŭ' => (letter('u'), BREVE),
        'Ů' | 'ů' => (letter('u'), RING),
        'Ű' | 'ű' => (letter('u'), DIAERESIS),
        'Ų' | 'ų' => (letter('u'), OGONEK),
        'Ŵ' | 'ŵ' => (letter('w'), CIRCUMFLEX),
        'Ŷ' | 'ŷ' => (letter('y'), CIRCUMFLEX),
        'Ÿ' => (letter('y'), DIAERESIS),
        'Ź' | 'ź' => (letter('z'), ACUTE),
        'Ż' | 'ż' => (letter('z'), DOT),
        'Ž' | 'ž' => (letter('z'), CARON),

        // Outside the table: compare by code point, case folded when the
        // script has a case at all.
        other => (letter(other.to_lowercase().next().unwrap_or(other)), NONE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(mut items: Vec<&str>) -> Vec<&str> {
        items.sort_by(|a, b| compare(a, b));
        items
    }

    /// The bug this exists for: byte order put every accented letter and every
    /// uppercase word in the wrong place.
    #[test]
    fn accents_and_case_no_longer_sort_after_z() {
        assert_eq!(
            sorted(vec!["Zebra", "arbol", "Ávila", "ñu", "acción"]),
            ["acción", "arbol", "Ávila", "ñu", "Zebra"]
        );
    }

    #[test]
    fn base_letters_decide_first() {
        assert_eq!(compare("ávila", "arbol"), Ordering::Greater); // v > r
        assert_eq!(compare("ávila", "zamora"), Ordering::Less); // a < z
        assert_eq!(compare("ñu", "oso"), Ordering::Less); // ñ < o
        assert_eq!(compare("ñu", "mesa"), Ordering::Greater); // ñ > m
    }

    /// `ñ` is a letter, not an accented `n`: every word starting with `n`
    /// precedes every word starting with `ñ`, which is what a Spanish reader
    /// expects and what the language-neutral Unicode default gets wrong.
    #[test]
    fn spanish_treats_enye_as_its_own_letter() {
        assert_eq!(
            sorted(vec!["ñu", "nube", "Ñandú", "orilla", "nabo"]),
            ["nabo", "nube", "Ñandú", "ñu", "orilla"]
        );
        assert_eq!(compare("nube", "ñu"), Ordering::Less);
        assert_eq!(compare("ñu", "orilla"), Ordering::Less);
        // Ñ and ñ still differ only by case.
        assert_eq!(compare("ñu", "Ñu"), Ordering::Less);
    }

    /// Diacritics only break ties, so accented and unaccented spellings of the
    /// same word stay adjacent instead of scattering.
    #[test]
    fn diacritics_break_ties_in_a_stable_order() {
        assert_eq!(
            sorted(vec!["ánimo", "animo", "àbaco"]),
            ["àbaco", "animo", "ánimo"]
        );
        assert_eq!(
            sorted(vec!["a", "ä", "á", "à", "å"]),
            ["a", "á", "à", "ä", "å"]
        );
        assert_eq!(compare("nu", "ñu"), Ordering::Less);
    }

    /// Lowercase first, then capitalized, then all caps: the case levels are
    /// compared position by position like the ones above them.
    #[test]
    fn case_is_the_last_resort() {
        assert_eq!(
            sorted(vec!["ARBOL", "arbol", "Arbol"]),
            ["arbol", "Arbol", "ARBOL"]
        );
        // and never outranks a base-letter difference
        assert_eq!(compare("Arbol", "banana"), Ordering::Less);
    }

    #[test]
    fn identical_strings_compare_equal() {
        assert_eq!(compare("acción! ñato", "acción! ñato"), Ordering::Equal);
        assert_eq!(compare("", ""), Ordering::Equal);
        assert_eq!(compare("", "a"), Ordering::Less);
    }

    /// Digits, spaces and punctuation keep a stable place below the letters.
    #[test]
    fn non_letters_compare_by_code_point() {
        assert_eq!(
            sorted(vec!["b", "1", "a", " ", "_"]),
            [" ", "1", "_", "a", "b"]
        );
    }

    /// Scripts with no Latin base still sort deterministically.
    #[test]
    fn other_scripts_are_ordered_not_grouped_arbitrarily() {
        assert_eq!(
            sorted(vec!["日本", "Ωμέγα", "zeta"]),
            ["zeta", "Ωμέγα", "日本"]
        );
        assert_eq!(compare("日本", "日本"), Ordering::Equal);
    }

    #[test]
    fn binary_collation_still_available_and_is_the_old_behavior() {
        let mut items = vec!["Zebra", "arbol", "Ávila", "ñu"];
        items.sort_by(|a, b| Collation::Binary.compare(a, b));
        assert_eq!(items, ["Zebra", "arbol", "Ávila", "ñu"]);
    }

    #[test]
    fn collation_names_round_trip() {
        assert_eq!(Collation::parse("unicode"), Some(Collation::Unicode));
        assert_eq!(Collation::parse("BINARY"), Some(Collation::Binary));
        assert_eq!(Collation::parse("es_ES"), None);
        assert_eq!(Collation::default(), Collation::Unicode);
        assert_eq!(Collation::Unicode.name(), "unicode");
    }

    /// A comparator that is not a total order corrupts a merge sort, so the
    /// three levels have to stay antisymmetric and transitive.
    #[test]
    fn the_comparator_is_a_total_order() {
        let items = [
            "", "a", "A", "á", "Á", "à", "ñ", "n", "no", "ñu", "nu", "z", "Z", "1", " ", "日", "ß",
            "s", "ss", "Œ", "o",
        ];
        for x in items {
            assert_eq!(compare(x, x), Ordering::Equal, "{x:?} vs itself");
            for y in items {
                assert_eq!(
                    compare(x, y),
                    compare(y, x).reverse(),
                    "antisymmetry for {x:?} / {y:?}"
                );
                for z in items {
                    if compare(x, y) == Ordering::Less && compare(y, z) == Ordering::Less {
                        assert_eq!(
                            compare(x, z),
                            Ordering::Less,
                            "transitivity for {x:?} < {y:?} < {z:?}"
                        );
                    }
                }
            }
        }
    }
}
