//! From the model's token stream to readable, timed segments.
//!
//! GigaAM's CTC head reports only tokens: SentencePiece pieces, where `▁`
//! marks the start of a word, each with the time of the 40 ms frame it was
//! emitted on. The text already carries punctuation and capitals, so segments
//! are cut where a sentence ends, at long pauses, and before they grow too
//! long to read.

use crate::Segment;

/// A pause at least this long ends a segment...
const PAUSE_MS: i64 = 1_000;
/// ...that has at least this many words: a hesitation after one or two words
/// ("I... lost my sister") is not worth a segment of its own, and translating
/// such a fragment alone goes wrong.
const MIN_WORDS_BEFORE_PAUSE: usize = 3;
/// A segment is cut at the next word boundary once it is this long.
const MAX_SEGMENT_MS: i64 = 20_000;
/// Tokens of one word lie closer together than this. A larger gap inside a
/// word means one piece was emitted out of place: a first letter on the very
/// first frame (the word's start then moves to its later pieces), or closing
/// punctuation only after the silence that follows (it is attached without
/// stretching the word).
const MAX_GAP_IN_WORD_MS: i64 = 400;
/// The model's frame length: a token spans at most this.
const FRAME_MS: i64 = 40;

/// One token as the model reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
}

struct Word {
    start_ms: i64,
    end_ms: i64,
    /// With its leading space, if it had one.
    text: String,
}

fn words(tokens: &[Token]) -> Vec<Word> {
    let mut words: Vec<Word> = Vec::new();
    for token in tokens {
        let starts_word = token.text.starts_with('▁');
        let text = token.text.replace('▁', " ");
        match words.last_mut() {
            Some(word) if !starts_word => {
                let late = token.start_ms - word.end_ms > MAX_GAP_IN_WORD_MS;
                let punctuation = !text.chars().any(char::is_alphanumeric);
                if late && punctuation {
                    word.text.push_str(&text);
                    continue;
                }
                if late {
                    word.start_ms = token.start_ms - FRAME_MS;
                }
                word.end_ms = token.end_ms;
                word.text.push_str(&text);
            }
            _ => words.push(Word {
                start_ms: token.start_ms,
                end_ms: token.end_ms,
                text,
            }),
        }
    }
    // A bare `▁` token followed by its pieces is still one word; a word whose
    // first piece is a lone space takes its timing from what follows.
    words.retain(|w| !w.text.trim().is_empty());
    words
}

fn ends_sentence(text: &str) -> bool {
    text.trim_end().ends_with(['.', '!', '?', '…'])
}

fn starts_sentence(text: &str) -> bool {
    text.trim_start().chars().next().is_some_and(|c| {
        c.is_uppercase() || c.is_ascii_digit() || matches!(c, '«' | '"' | '—' | '-')
    })
}

/// Splits a token stream into segments. `offset_ms` is added to every time,
/// to place the result on the recording's timeline.
pub fn segments_from_tokens(tokens: &[Token], offset_ms: i64) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut current: Vec<Word> = Vec::new();

    let flush = |current: &mut Vec<Word>, segments: &mut Vec<Segment>| {
        let (Some(first), Some(last)) = (current.first(), current.last()) else {
            return;
        };
        let text: String = current.iter().map(|w| w.text.as_str()).collect();
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if !text.is_empty() {
            segments.push(Segment {
                start_ms: offset_ms + first.start_ms,
                end_ms: offset_ms + last.end_ms.max(first.start_ms),
                text,
            });
        }
        current.clear();
    };

    for word in words(tokens) {
        if let (Some(first), Some(previous)) = (current.first(), current.last()) {
            let boundary = word.text.starts_with(' ');
            let paused = word.start_ms - previous.end_ms >= PAUSE_MS
                && (current.len() >= MIN_WORDS_BEFORE_PAUSE || ends_sentence(&previous.text));
            let sentence = ends_sentence(&previous.text) && starts_sentence(&word.text);
            let too_long = word.end_ms - first.start_ms > MAX_SEGMENT_MS;
            if boundary && (paused || sentence || too_long) {
                flush(&mut current, &mut segments);
            }
        }
        current.push(word);
    }
    flush(&mut current, &mut segments);
    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tokens laid out one frame apart from `start_ms`.
    fn tokens(start_ms: i64, pieces: &[&str]) -> Vec<Token> {
        pieces
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let t = start_ms + i as i64 * 80;
                Token {
                    start_ms: t,
                    end_ms: t + FRAME_MS,
                    text: (*p).into(),
                }
            })
            .collect()
    }

    #[test]
    fn joins_pieces_into_words_and_splits_at_sentence_ends() {
        let toks = tokens(
            0,
            &[
                "▁Д",
                "об",
                "рый",
                "▁день",
                ".",
                "▁",
                "На",
                "ч",
                "нём",
                "▁8",
                ".",
                "5",
                "▁раз",
                ".",
            ],
        );
        let segments = segments_from_tokens(&toks, 10_000);
        let texts: Vec<_> = segments.iter().map(|s| s.text.as_str()).collect();
        // "8.5" is a number, not a sentence end: no capital follows.
        assert_eq!(texts, ["Добрый день.", "Начнём 8.5 раз."]);
        assert_eq!(segments[0].start_ms, 10_000);
        assert_eq!(segments[0].end_ms, 10_000 + 4 * 80 + FRAME_MS);
        assert_eq!(segments[1].start_ms, 10_000 + 5 * 80);
    }

    #[test]
    fn a_long_pause_ends_a_segment_even_mid_sentence() {
        let mut toks = tokens(0, &["▁и", "▁потом", "▁опять"]);
        toks.extend(tokens(3_000, &["▁мы", "▁пошли"]));
        let segments = segments_from_tokens(&toks, 0);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[1].text, "мы пошли");
        assert_eq!(segments[1].start_ms, 3_000);
    }

    #[test]
    fn a_hesitation_after_a_word_or_two_does_not_split() {
        let mut toks = tokens(0, &["▁Я"]);
        toks.extend(tokens(1_500, &["▁потерял", "▁сестру"]));
        let segments = segments_from_tokens(&toks, 0);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, "Я потерял сестру");
    }

    #[test]
    fn an_early_first_piece_does_not_stretch_the_word_back() {
        // As seen from GigaAM: "▁О" on frame 0, the rest 2.5 s later.
        let toks = vec![
            Token {
                start_ms: 0,
                end_ms: 40,
                text: "▁О".into(),
            },
            Token {
                start_ms: 2_560,
                end_ms: 2_600,
                text: "с".into(),
            },
            Token {
                start_ms: 2_640,
                end_ms: 2_680,
                text: "но".into(),
            },
        ];
        let segments = segments_from_tokens(&toks, 0);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, "Осно");
        assert_eq!(segments[0].start_ms, 2_520);
    }

    #[test]
    fn late_closing_punctuation_stays_with_its_word() {
        // As seen from GigaAM: the full stop comes after 7 s of silence.
        let mut toks = tokens(5_600, &["▁хри", "сти", "ан", "ство"]);
        toks.push(Token {
            start_ms: 12_960,
            end_ms: 13_000,
            text: ".".into(),
        });
        toks.extend(tokens(13_200, &["▁Я", "▁потерял"]));
        let segments = segments_from_tokens(&toks, 0);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].text, "христианство.");
        assert_eq!(
            (segments[0].start_ms, segments[0].end_ms),
            (5_600, 5_600 + 3 * 80 + FRAME_MS)
        );
        assert_eq!(segments[1].text, "Я потерял");
    }

    #[test]
    fn endless_sentences_are_cut_at_word_boundaries() {
        let pieces: Vec<String> = (0..600).map(|i| format!("▁w{i}")).collect();
        let refs: Vec<&str> = pieces.iter().map(String::as_str).collect();
        let segments = segments_from_tokens(&tokens(0, &refs), 0);
        assert!(segments.len() >= 2);
        for s in &segments {
            assert!(s.end_ms - s.start_ms <= MAX_SEGMENT_MS, "{s:?}");
        }
        let words: usize = segments.iter().map(|s| s.text.split(' ').count()).sum();
        assert_eq!(words, 600);
    }

    #[test]
    fn nothing_in_nothing_out() {
        assert!(segments_from_tokens(&[], 0).is_empty());
        assert!(segments_from_tokens(&tokens(0, &["▁"]), 0).is_empty());
    }
}
