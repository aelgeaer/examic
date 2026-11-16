//! A SentencePiece unigram tokenizer in plain Rust.
//!
//! Reads the `.spm` model file (a protobuf) directly and segments text the way
//! libsentencepiece's unigram model does: normalise, mark word starts with
//! `▁`, then pick the segmentation with the highest total piece score
//! (Viterbi). Piece strings are mapped to model ids through Marian's
//! `vocab.json`, as `MarianTokenizer` does.

use std::collections::HashMap;
use std::path::Path;

use unicode_normalization::UnicodeNormalization;

/// The word-start marker.
const SPACE: char = '▁';
/// libsentencepiece scores an unknown character this far below the worst piece.
const UNK_PENALTY: f32 = 10.0;

#[derive(Debug, thiserror::Error)]
pub enum SpmError {
    #[error("cannot read {0}: {1}")]
    Io(String, std::io::Error),
    #[error("malformed SentencePiece model: {0}")]
    Model(&'static str),
    #[error("unsupported SentencePiece model: {0}")]
    Unsupported(&'static str),
    #[error("malformed vocabulary: {0}")]
    Vocab(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PieceType {
    Normal,
    Unknown,
    Control,
    UserDefined,
    Unused,
    Byte,
}

impl PieceType {
    fn from_proto(value: u64) -> Self {
        match value {
            2 => Self::Unknown,
            3 => Self::Control,
            4 => Self::UserDefined,
            5 => Self::Unused,
            6 => Self::Byte,
            _ => Self::Normal,
        }
    }
}

struct Piece {
    text: String,
    score: f32,
    kind: PieceType,
}

/// Normaliser settings from the model's `NormalizerSpec`.
struct Normalizer {
    nfkc: bool,
    add_dummy_prefix: bool,
    remove_extra_whitespaces: bool,
}

pub struct SpmTokenizer {
    /// Candidate pieces for segmentation, by text: (score, is user-defined).
    pieces: HashMap<String, (f32, bool)>,
    /// Longest candidate, in chars.
    max_piece_chars: usize,
    unk_score: f32,
    max_score: f32,
    normalizer: Normalizer,
    vocab: HashMap<String, i64>,
    id_to_piece: HashMap<i64, String>,
    unk_id: i64,
    eos_id: i64,
    /// Ids never shown in decoded text (`</s>`, `<pad>`, `<unk>`, `<s>`).
    special_ids: Vec<i64>,
}

impl SpmTokenizer {
    /// Loads `spm.model` and Marian's `vocab.json`.
    pub fn load(spm_path: &Path, vocab_path: &Path) -> Result<Self, SpmError> {
        let read =
            |p: &Path| std::fs::read(p).map_err(|e| SpmError::Io(p.display().to_string(), e));
        Self::from_bytes(&read(spm_path)?, &read(vocab_path)?)
    }

    pub fn from_bytes(spm: &[u8], vocab_json: &[u8]) -> Result<Self, SpmError> {
        let model = parse_model(spm)?;
        let vocab: HashMap<String, i64> = match serde_json::from_slice(vocab_json) {
            Ok(serde_json::Value::Object(map)) => map
                .into_iter()
                .map(|(k, v)| match v.as_i64() {
                    Some(id) => Ok((k, id)),
                    None => Err(SpmError::Vocab(k)),
                })
                .collect::<Result<_, _>>()?,
            Ok(_) => return Err(SpmError::Vocab("not an object".into())),
            Err(e) => return Err(SpmError::Vocab(e.to_string())),
        };
        let id_of = |piece: &str| {
            vocab
                .get(piece)
                .copied()
                .ok_or_else(|| SpmError::Vocab(format!("no {piece}")))
        };
        let unk_id = id_of("<unk>")?;
        let eos_id = id_of("</s>")?;
        let special_ids = ["</s>", "<unk>", "<pad>", "<s>"]
            .iter()
            .filter_map(|p| vocab.get(*p).copied())
            .collect();

        let candidates: Vec<&Piece> = model
            .pieces
            .iter()
            .filter(|p| matches!(p.kind, PieceType::Normal | PieceType::UserDefined))
            .collect();
        let min_score = candidates
            .iter()
            .map(|p| p.score)
            .fold(f32::INFINITY, f32::min);
        let max_score = candidates
            .iter()
            .map(|p| p.score)
            .fold(f32::NEG_INFINITY, f32::max);
        Ok(Self {
            max_piece_chars: candidates
                .iter()
                .map(|p| p.text.chars().count())
                .max()
                .unwrap_or(1),
            pieces: candidates
                .iter()
                .map(|p| (p.text.clone(), (p.score, p.kind == PieceType::UserDefined)))
                .collect(),
            unk_score: min_score - UNK_PENALTY,
            max_score,
            normalizer: model.normalizer,
            id_to_piece: vocab.iter().map(|(k, &v)| (v, k.clone())).collect(),
            vocab,
            unk_id,
            eos_id,
            special_ids,
        })
    }

    pub fn eos_id(&self) -> i64 {
        self.eos_id
    }

    /// Normalises text and turns spaces into word-start markers.
    fn normalize(&self, text: &str) -> String {
        let text: String = if self.normalizer.nfkc {
            text.nfkc().collect()
        } else {
            text.to_owned()
        };
        // Control characters carry nothing a translation could use.
        let text: String = text
            .chars()
            .map(|c| if c.is_whitespace() { ' ' } else { c })
            .filter(|c| !c.is_control())
            .collect();
        let text = if self.normalizer.remove_extra_whitespaces {
            text.split(' ')
                .filter(|w| !w.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            text
        };
        let mut out = String::with_capacity(text.len() + 3);
        if self.normalizer.add_dummy_prefix && !text.is_empty() {
            out.push(SPACE);
        }
        out.extend(text.chars().map(|c| if c == ' ' { SPACE } else { c }));
        out
    }

    /// Splits text into pieces.
    pub fn encode_pieces(&self, text: &str) -> Vec<String> {
        let chars: Vec<char> = self.normalize(text).chars().collect();
        let n = chars.len();
        // best[i]: (score, start of the last piece) of the best path to i.
        let mut best: Vec<(f32, usize)> = vec![(f32::NEG_INFINITY, 0); n + 1];
        best[0] = (0.0, 0);
        let mut candidate = String::new();
        for start in 0..n {
            let base = best[start].0;
            if base == f32::NEG_INFINITY {
                continue;
            }
            let mut single_char_known = false;
            candidate.clear();
            for len in 1..=self.max_piece_chars.min(n - start) {
                candidate.push(chars[start + len - 1]);
                if let Some(&(score, user_defined)) = self.pieces.get(&candidate) {
                    // libsentencepiece makes user-defined pieces always win.
                    let score = if user_defined {
                        len as f32 * self.max_score - 0.1
                    } else {
                        score
                    };
                    let end = start + len;
                    if base + score > best[end].0 {
                        best[end] = (base + score, start);
                    }
                    single_char_known |= len == 1;
                }
            }
            if !single_char_known && base + self.unk_score > best[start + 1].0 {
                best[start + 1] = (base + self.unk_score, start);
            }
        }
        let mut pieces = Vec::new();
        let mut end = n;
        while end > 0 {
            let start = best[end].1;
            pieces.push(chars[start..end].iter().collect());
            end = start;
        }
        pieces.reverse();
        pieces
    }

    /// Model input ids for `text`: its pieces, then `</s>`. At most `max_len`
    /// ids; longer input is cut (keeping the final `</s>`).
    pub fn encode(&self, text: &str, max_len: usize) -> Vec<i64> {
        let mut ids: Vec<i64> = self
            .encode_pieces(text)
            .iter()
            .map(|p| self.vocab.get(p).copied().unwrap_or(self.unk_id))
            .collect();
        ids.truncate(max_len.saturating_sub(1));
        ids.push(self.eos_id);
        ids
    }

    /// Text for output ids, special tokens left out.
    pub fn decode(&self, ids: &[i64]) -> String {
        let text: String = ids
            .iter()
            .filter(|id| !self.special_ids.contains(id))
            .filter_map(|id| self.id_to_piece.get(id))
            .map(String::as_str)
            .collect();
        text.replace(SPACE, " ").trim().to_owned()
    }
}

struct Model {
    pieces: Vec<Piece>,
    normalizer: Normalizer,
}

/// Minimal protobuf reading: just the fields this tokenizer needs.
struct Reader<'a> {
    data: &'a [u8],
}

enum Field<'a> {
    Varint(u64),
    Fixed32(u32),
    Fixed64,
    Bytes(&'a [u8]),
}

impl<'a> Reader<'a> {
    fn varint(&mut self) -> Result<u64, SpmError> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let (&byte, rest) = self
                .data
                .split_first()
                .ok_or(SpmError::Model("truncated varint"))?;
            self.data = rest;
            value |= u64::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(SpmError::Model("varint too long"))
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], SpmError> {
        if self.data.len() < n {
            return Err(SpmError::Model("truncated field"));
        }
        let (head, rest) = self.data.split_at(n);
        self.data = rest;
        Ok(head)
    }

    fn next(&mut self) -> Result<Option<(u64, Field<'a>)>, SpmError> {
        if self.data.is_empty() {
            return Ok(None);
        }
        let key = self.varint()?;
        let field = match key & 7 {
            0 => Field::Varint(self.varint()?),
            1 => {
                self.take(8)?;
                Field::Fixed64
            }
            2 => {
                let len = self.varint()? as usize;
                Field::Bytes(self.take(len)?)
            }
            5 => Field::Fixed32(u32::from_le_bytes(self.take(4)?.try_into().unwrap())),
            _ => return Err(SpmError::Model("unsupported wire type")),
        };
        Ok(Some((key >> 3, field)))
    }
}

fn parse_model(data: &[u8]) -> Result<Model, SpmError> {
    let mut pieces = Vec::new();
    let mut normalizer = Normalizer {
        nfkc: true,
        add_dummy_prefix: true,
        remove_extra_whitespaces: true,
    };
    let mut reader = Reader { data };
    while let Some((number, field)) = reader.next()? {
        match (number, field) {
            (1, Field::Bytes(bytes)) => pieces.push(parse_piece(bytes)?),
            (2, Field::Bytes(bytes)) => check_trainer(bytes)?,
            (3, Field::Bytes(bytes)) => normalizer = parse_normalizer(bytes)?,
            _ => {}
        }
    }
    if pieces.is_empty() {
        return Err(SpmError::Model("no pieces"));
    }
    Ok(Model { pieces, normalizer })
}

fn parse_piece(data: &[u8]) -> Result<Piece, SpmError> {
    let mut piece = Piece {
        text: String::new(),
        score: 0.0,
        kind: PieceType::Normal,
    };
    let mut reader = Reader { data };
    while let Some((number, field)) = reader.next()? {
        match (number, field) {
            (1, Field::Bytes(b)) => {
                piece.text = String::from_utf8(b.to_vec())
                    .map_err(|_| SpmError::Model("piece is not UTF-8"))?
            }
            (2, Field::Fixed32(bits)) => piece.score = f32::from_bits(bits),
            (3, Field::Varint(v)) => piece.kind = PieceType::from_proto(v),
            _ => {}
        }
    }
    Ok(piece)
}

fn check_trainer(data: &[u8]) -> Result<(), SpmError> {
    let mut reader = Reader { data };
    while let Some((number, field)) = reader.next()? {
        match (number, field) {
            // model_type: UNIGRAM = 1 (the default).
            (3, Field::Varint(v)) if v != 1 => {
                return Err(SpmError::Unsupported("not a unigram model"));
            }
            // treat_whitespace_as_suffix
            (24, Field::Varint(1)) => return Err(SpmError::Unsupported("whitespace as suffix")),
            _ => {}
        }
    }
    Ok(())
}

fn parse_normalizer(data: &[u8]) -> Result<Normalizer, SpmError> {
    let mut name = String::new();
    let mut normalizer = Normalizer {
        nfkc: true,
        add_dummy_prefix: true,
        remove_extra_whitespaces: true,
    };
    let mut reader = Reader { data };
    while let Some((number, field)) = reader.next()? {
        match (number, field) {
            (1, Field::Bytes(b)) => name = String::from_utf8_lossy(b).into_owned(),
            (3, Field::Varint(v)) => normalizer.add_dummy_prefix = v != 0,
            (4, Field::Varint(v)) => normalizer.remove_extra_whitespaces = v != 0,
            (5, Field::Varint(0)) => return Err(SpmError::Unsupported("unescaped whitespace")),
            _ => {}
        }
    }
    // "nmt_nfkc" and "nfkc" (and their case-folding variants) apply NFKC;
    // "identity" leaves text as it is.
    normalizer.nfkc = name != "identity";
    Ok(normalizer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let byte = (v & 0x7F) as u8;
            v >>= 7;
            if v == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    fn bytes_field(number: u64, data: &[u8], out: &mut Vec<u8>) {
        varint(number << 3 | 2, out);
        varint(data.len() as u64, out);
        out.extend_from_slice(data);
    }

    /// A model with the given (piece, score, type) entries.
    fn model(pieces: &[(&str, f32, u64)]) -> Vec<u8> {
        let mut out = Vec::new();
        for &(text, score, kind) in pieces {
            let mut p = Vec::new();
            bytes_field(1, text.as_bytes(), &mut p);
            varint(2 << 3 | 5, &mut p);
            p.extend_from_slice(&score.to_bits().to_le_bytes());
            varint(3 << 3, &mut p);
            varint(kind, &mut p);
            bytes_field(1, &p, &mut out);
        }
        let mut norm = Vec::new();
        bytes_field(1, b"nmt_nfkc", &mut norm);
        bytes_field(3, &norm, &mut out);
        out
    }

    fn tokenizer() -> SpmTokenizer {
        let spm = model(&[
            ("<unk>", 0.0, 2),
            ("</s>", 0.0, 3),
            ("▁", -2.0, 1),
            ("▁при", -3.0, 1),
            ("▁привет", -4.0, 1),
            ("вет", -3.0, 1),
            ("п", -5.0, 1),
            ("р", -5.0, 1),
            ("и", -5.0, 1),
            ("в", -5.0, 1),
            ("е", -5.0, 1),
            ("т", -5.0, 1),
            ("!", -3.0, 1),
        ]);
        let vocab = r#"{"</s>":0,"<unk>":1,"▁привет":2,"!":3,"▁при":4,"вет":5,"▁":6,"<pad>":7}"#;
        SpmTokenizer::from_bytes(&spm, vocab.as_bytes()).unwrap()
    }

    #[test]
    fn picks_the_best_scoring_segmentation() {
        let t = tokenizer();
        // "▁привет" (-4) beats "▁при"+"вет" (-6) and anything finer.
        assert_eq!(t.encode_pieces("привет!"), ["▁привет", "!"]);
        assert_eq!(t.encode("  привет\t привет!  ", 64), [2, 2, 3, 0]);
    }

    #[test]
    fn unknown_characters_become_unk() {
        let t = tokenizer();
        assert_eq!(t.encode_pieces("при😀"), ["▁при", "😀"]);
        assert_eq!(t.encode("при😀", 64), [4, 1, 0]);
    }

    #[test]
    fn decodes_and_hides_special_tokens() {
        let t = tokenizer();
        assert_eq!(t.decode(&[7, 2, 2, 3, 0]), "привет привет!");
    }

    #[test]
    fn long_input_keeps_its_end_marker() {
        let t = tokenizer();
        let ids = t.encode(&"привет ".repeat(100), 10);
        assert_eq!(ids.len(), 10);
        assert_eq!(ids.last(), Some(&0));
    }
}
