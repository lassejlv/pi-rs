//! Text files embedded in the binary deflate-compressed: the bundled JavaScript, model catalog
//! and schemas are several megabytes as plain text and a fraction of that compressed.
//!
//! [`include_str!`] replaces `std::include_str!` and yields an [`Embedded`]; [`Embedded::get`]
//! inflates the text once and keeps it for the rest of the process.

use std::io::Read as _;
use std::sync::OnceLock;

pub use pi_embed_macros::include_str;

// The macro names `::pi_embed`, which this crate's own tests use too.
extern crate self as pi_embed;

/// A compressed embedded text file. `Copy` and const-constructible, so it fits in `const`
/// tables where a `&'static str` did.
#[derive(Clone, Copy)]
pub struct Embedded {
    compressed: &'static [u8],
    len: usize,
    cell: &'static OnceLock<Box<str>>,
}

impl Embedded {
    #[doc(hidden)]
    pub const fn __new(compressed: &'static [u8], len: usize, cell: &'static OnceLock<Box<str>>) -> Self {
        Self { compressed, len, cell }
    }

    /// The text, inflated on first call and kept for the life of the process.
    pub fn get(self) -> &'static str {
        self.cell.get_or_init(|| self.decompress().into_boxed_str())
    }

    /// A fresh copy of the text, not kept: for data parsed once into another form.
    pub fn decompress(self) -> String {
        if let Some(text) = self.cell.get() {
            return text.to_string();
        }
        let mut text = String::with_capacity(self.len);
        flate2::read::DeflateDecoder::new(self.compressed)
            .read_to_string(&mut text)
            .expect("embedded text inflates to UTF-8");
        text
    }
}

impl std::fmt::Debug for Embedded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Embedded").field("len", &self.len).field("compressed", &self.compressed.len()).finish()
    }
}

#[cfg(test)]
mod tests {
    const TABLE: &[(&str, super::Embedded)] = &[("lib", crate::include_str!("lib.rs"))];

    #[test]
    fn round_trips_and_caches() {
        let embedded = TABLE[0].1;
        assert_eq!(embedded.decompress(), std::include_str!("lib.rs"));
        let first = embedded.get();
        assert_eq!(first, std::include_str!("lib.rs"));
        assert!(std::ptr::eq(first, TABLE[0].1.get()));
        assert!(embedded.compressed.len() < first.len());
    }
}
