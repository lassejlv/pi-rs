//! `String.prototype.localeCompare` as V8 implements it: ICU's root collation (CLDR) at tertiary
//! strength with non-ignorable punctuation, which is what `a.localeCompare(b)` uses for the
//! default `en`/`und` locales. Backed by ICU4X's collator with compiled CLDR data.

use std::cmp::Ordering;
use std::sync::OnceLock;

use icu_collator::options::CollatorOptions;
use icu_collator::{Collator, CollatorBorrowed};

fn collator() -> &'static CollatorBorrowed<'static> {
    static COLLATOR: OnceLock<CollatorBorrowed<'static>> = OnceLock::new();
    COLLATOR.get_or_init(|| {
        Collator::try_new(Default::default(), CollatorOptions::default()).expect("compiled root collation data")
    })
}

/// `a.localeCompare(b)` with the root collation.
pub fn locale_compare(a: &str, b: &str) -> Ordering {
    collator().compare(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_like_v8_locale_compare() {
        // Node 22: v.sort((x, y) => x.localeCompare(y))
        let mut v = vec!["b", "_a", "A", "a", ".hidden", "10", "2", "a-b", "a_b", "é", "Z", "z", "a.b", "ab", "a b"];
        v.sort_by(|a, b| locale_compare(a, b));
        assert_eq!(v, vec!["_a", ".hidden", "10", "2", "a", "A", "a b", "a_b", "a-b", "a.b", "ab", "b", "é", "z", "Z"]);
    }
}
