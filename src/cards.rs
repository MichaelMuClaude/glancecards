//! Which cards there are and in what order they come, shared by the window and the web page.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rand::seq::SliceRandom;

/// Where cards you already know go: the `known` setting, or a `known` folder next to the first card folder.
pub fn known_dir(known: Option<&Path>, dirs: &[PathBuf]) -> PathBuf {
    known.map(Path::to_path_buf).unwrap_or_else(|| {
        let first = dirs.first().cloned().unwrap_or_else(|| PathBuf::from("."));
        first.parent().map(|p| p.join("known")).unwrap_or_else(|| first.join("known"))
    })
}

/// "apple.png" becomes "apple (1).png" (or the first free number) when the name is taken.
pub fn unique_path(p: &Path) -> PathBuf {
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let dir = p.parent().unwrap_or(Path::new("."));
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|c| !c.exists())
        .expect("an unused name exists")
}

/// Sentence cards are named "sentence_<id>_<key-phrase>.png".
pub fn is_sentence(p: &Path) -> bool {
    p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("sentence_"))
}

/// Cards you are learning right now, shown more often: sentence cards and the words you looked up
/// yourself ("mine_<word>.png").
pub fn is_featured(p: &Path) -> bool {
    is_sentence(p) || p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("mine_"))
}

/// Shuffles the cards for a new round and spreads the featured cards (sentences and your own words)
/// through it: one after every `every` other cards, going round them again and again, so a handful come
/// back often instead of once in thousands of cards. `every` = 0 shuffles them in like any other card.
pub fn arrange(cards: Vec<PathBuf>, every: usize) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let (mut sentences, mut words): (Vec<_>, Vec<_>) =
        cards.into_iter().filter(|p| seen.insert(p.clone())).partition(|p| is_featured(p));
    let rng = &mut rand::rng();
    if every == 0 || sentences.is_empty() || words.is_empty() {
        words.append(&mut sentences);
        words.shuffle(rng);
        return words;
    }
    sentences.shuffle(rng);
    words.shuffle(rng);
    let n = words.len();
    let slots = (n / every).max(sentences.len());
    let mut out = Vec::with_capacity(n + slots);
    let mut placed = 0;
    for (i, w) in words.into_iter().enumerate() {
        out.push(w);
        while placed < slots && (placed + 1) * n / slots <= i + 1 {
            out.push(sentences[placed % sentences.len()].clone());
            placed += 1;
        }
    }
    out
}

/// The word a card shows, taken from its file name: "apple.png", "wa_according-to.jpg",
/// or "apple (2).png" after a name clash in the known folder. A sentence card gives its key phrase
/// ("sentence_005_figure-out.png" is "figure out"); one of your own words drops its prefix
/// ("mine_verge-collection.png" is "verge collection").
pub fn card_word(p: &Path) -> Option<String> {
    let stem = p.file_stem()?.to_string_lossy();
    let own = stem.strip_prefix("mine_");
    if let Some(rest) = own.or_else(|| stem.strip_prefix("sentence_")) {
        let phrase = if own.is_some() { rest } else { rest.split_once('_').map_or(rest, |(_, k)| k) };
        let phrase = phrase.split(" (").next().unwrap_or(phrase).replace('-', " ");
        let phrase = phrase.trim();
        return (!phrase.is_empty()).then(|| phrase.to_string());
    }
    let stem = stem.strip_prefix("wa_").unwrap_or(&stem);
    let stem = match stem.rfind(" (") {
        Some(i) if stem.ends_with(')') => &stem[..i],
        _ => stem,
    };
    let word = stem.trim();
    (!word.is_empty()).then(|| word.to_string())
}

/// Cambridge's direct search jumps to the entry, or to its spelling suggestions when there is none.
pub fn cambridge_url(word: &str) -> String {
    let q: String = word
        .bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' => (b as char).to_string(),
            b' ' => "+".to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect();
    format!("https://dictionary.cambridge.org/search/direct/?datasetsearch=english-chinese-simplified&q={q}")
}

/// Renames, or copies and deletes when the folders are on different drives.
pub fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to)?;
    if let Err(e) = std::fs::remove_file(from) {
        // Leave one copy, not two: the card stays where it was.
        let _ = std::fs::remove_file(to);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_from_file_name() {
        assert_eq!(card_word(Path::new("/x/apple.png")).as_deref(), Some("apple"));
        assert_eq!(card_word(Path::new("/x/wa_according-to.jpg")).as_deref(), Some("according-to"));
        assert_eq!(card_word(Path::new("/x/apple (2).png")).as_deref(), Some("apple"));
        assert_eq!(card_word(Path::new("/x/wa_.jpg")), None);
        assert_eq!(card_word(Path::new("/x/sentence_005_figure-out.png")).as_deref(), Some("figure out"));
        assert_eq!(card_word(Path::new("/x/sentence_002_take-a-break (1).png")).as_deref(), Some("take a break"));
        assert_eq!(card_word(Path::new("/x/mine_verge-collection.png")).as_deref(), Some("verge collection"));
        assert_eq!(card_word(Path::new("/x/mine_tradie.jpg")).as_deref(), Some("tradie"));
        assert!(is_featured(Path::new("/x/mine_tradie.png")) && !is_sentence(Path::new("/x/mine_tradie.png")));
    }

    #[test]
    fn sentences_are_spread_through_the_round() {
        let words: Vec<PathBuf> = (0..100).map(|i| PathBuf::from(format!("w{i}.png"))).collect();
        let sentences: Vec<PathBuf> = (1..=3).map(|i| PathBuf::from(format!("sentence_00{i}_x.png"))).collect();
        let all: Vec<PathBuf> = words.iter().chain(&sentences).cloned().collect();
        let q = arrange(all.clone(), 5);
        assert_eq!(q.len(), 120);
        assert_eq!(q.iter().filter(|p| !is_sentence(p)).count(), 100);
        // Never two sentences in a row, and every sentence comes back several times.
        assert!(q.windows(2).all(|w| !(is_sentence(&w[0]) && is_sentence(&w[1]))));
        assert!(sentences.iter().all(|s| q.iter().filter(|p| *p == s).count() >= 6));
        // A round that already holds repeats is rebuilt from one copy of each card.
        assert_eq!(arrange(q, 5).len(), 120);
        // 0 turns it off; fewer words than `every` still shows each sentence once.
        assert_eq!(arrange(all, 0).len(), 103);
        assert_eq!(arrange(vec![words[0].clone()].into_iter().chain(sentences.clone()).collect(), 5).len(), 4);
    }

    #[test]
    fn cambridge_query_is_encoded() {
        assert!(cambridge_url("according-to").ends_with("&q=according-to"));
        assert!(cambridge_url("ice cream").ends_with("&q=ice+cream"));
        assert!(cambridge_url("o'clock&x").ends_with("&q=o%27clock%26x"));
        assert!(cambridge_url("café").ends_with("&q=caf%C3%A9"));
    }

    #[test]
    fn move_file_moves_once() {
        let dir = std::env::temp_dir().join(format!("glancecards-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.png"), dir.join("b.png"));
        std::fs::write(&a, b"x").unwrap();
        move_file(&a, &b).unwrap();
        assert!(!a.exists() && b.exists());
        assert_eq!(unique_path(&b), dir.join("b (1).png"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
