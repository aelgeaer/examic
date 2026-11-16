//! Sentence splitting for punctuated ASR output.

/// Splits text after `.`, `!`, `?` or `…` when the next word starts like a
/// sentence (a capital letter, a digit or an opening quote or dash). Numbers
/// such as `8.5` and lower-case continuations stay together.
pub fn split_sentences(text: &str) -> Vec<&str> {
    let mut sentences = Vec::new();
    let mut start = 0;
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    for (i, &(at, c)) in chars.iter().enumerate() {
        if at < start || !matches!(c, '.' | '!' | '?' | '…') {
            continue;
        }
        // Include any run of closing punctuation (`?!`, `...`, `»`).
        let mut j = i + 1;
        while j < chars.len() && matches!(chars[j].1, '.' | '!' | '?' | '…' | '»' | '"' | ')') {
            j += 1;
        }
        if j >= chars.len() || !chars[j].1.is_whitespace() {
            continue;
        }
        let mut k = j;
        while k < chars.len() && chars[k].1.is_whitespace() {
            k += 1;
        }
        let starts_sentence = chars.get(k).is_some_and(|&(_, n)| {
            n.is_uppercase() || n.is_ascii_digit() || matches!(n, '«' | '"' | '—' | '-')
        });
        if starts_sentence {
            let end = chars[j].0;
            let sentence = text[start..end].trim();
            if !sentence.is_empty() {
                sentences.push(sentence);
            }
            start = chars[k].0;
        }
    }
    let rest = text[start..].trim();
    if !rest.is_empty() {
        sentences.push(rest);
    }
    sentences
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_at_sentence_ends_only() {
        assert_eq!(
            split_sentences("Добрый день. Начнём?! «Да», — сказал он. Курс 8.5 рубля… и всё."),
            [
                "Добрый день.",
                "Начнём?!",
                "«Да», — сказал он.",
                "Курс 8.5 рубля… и всё."
            ]
        );
        assert_eq!(split_sentences("Т. е. ничего"), ["Т. е. ничего"]);
        assert_eq!(split_sentences("Одно предложение"), ["Одно предложение"]);
        assert!(split_sentences("  ").is_empty());
    }
}
