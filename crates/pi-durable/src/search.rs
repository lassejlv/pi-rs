//! Stable BM25 ranking shared by tool discovery and code mode.
use crate::JsonString;
use indexmap::{IndexMap, IndexSet};

#[derive(Clone, Debug)]
pub struct SearchDocument {
    pub name: JsonString,
    pub text: JsonString,
}

const STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "is", "it", "of", "on",
    "or", "that", "the", "this", "to", "with",
];

fn stem(term: &str) -> String {
    if term.len() > 4 && term.ends_with("ies") {
        return format!("{}y", &term[..term.len() - 3]);
    }
    if term.len() > 4
        && ["ches", "shes", "sses", "xes", "zes"]
            .iter()
            .any(|s| term.ends_with(s))
    {
        return term[..term.len() - 2].into();
    }
    if term.len() > 3 && term.ends_with('s') && !term.ends_with("ss") {
        return term[..term.len() - 1].into();
    }
    term.into()
}

pub fn tokenize(text: &JsonString) -> Vec<String> {
    // Unpaired UTF16 units split terms, just as the source's non-ASCII regexp.
    let chars: Vec<char> = text.to_string_lossy().chars().collect();
    let mut separated = String::new();
    for (index, &ch) in chars.iter().enumerate() {
        if index > 0 && ch.is_ascii_uppercase() {
            let previous = chars[index - 1];
            if previous.is_ascii_lowercase()
                || previous.is_ascii_digit()
                || previous.is_ascii_uppercase()
                    && chars.get(index + 1).is_some_and(char::is_ascii_lowercase)
            {
                separated.push(' ');
            }
        }
        separated.push(ch);
    }
    separated
        .to_lowercase()
        .split(|ch: char| !ch.is_ascii_lowercase() && !ch.is_ascii_digit())
        .filter(|term| !term.is_empty() && !STOP_WORDS.contains(term))
        .map(stem)
        .collect()
}

pub fn rank_documents(
    query: &JsonString,
    documents: &[SearchDocument],
    limit: f64,
) -> Vec<JsonString> {
    let query: IndexSet<String> = tokenize(query).into_iter().collect();
    if query.is_empty() || documents.is_empty() || limit <= 0.0 || limit.is_nan() {
        return Vec::new();
    }
    let counts: Vec<IndexMap<String, usize>> = documents
        .iter()
        .map(|document| {
            let mut counts = IndexMap::new();
            for term in tokenize(&document.text) {
                *counts.entry(term).or_default() += 1;
            }
            counts
        })
        .collect();
    let lengths: Vec<usize> = counts.iter().map(|counts| counts.values().sum()).collect();
    let average = lengths.iter().sum::<usize>() as f64 / documents.len() as f64;
    let average = if average == 0.0 { 1.0 } else { average };
    let idf: Vec<f64> = query
        .iter()
        .map(|term| {
            let frequency = counts
                .iter()
                .filter(|counts| counts.contains_key(term))
                .count() as f64;
            (1.0 + (documents.len() as f64 - frequency + 0.5) / (frequency + 0.5)).ln()
        })
        .collect();
    let mut matches: Vec<(&SearchDocument, f64)> = documents
        .iter()
        .enumerate()
        .filter_map(|(index, document)| {
            let mut score = 0.0;
            for (term, idf) in query.iter().zip(&idf) {
                if let Some(&count) = counts[index].get(term) {
                    let norm = 1.2 * (1.0 - 0.75 + (0.75 * lengths[index] as f64) / average);
                    score += idf * ((count as f64 * (1.2 + 1.0)) / (count as f64 + norm));
                }
            }
            (score > 0.0).then_some((document, score))
        })
        .collect();
    matches.sort_by(|left, right| right.1.total_cmp(&left.1));
    matches
        .into_iter()
        .take(limit as usize)
        .map(|(document, _)| document.name.clone())
        .collect()
}
