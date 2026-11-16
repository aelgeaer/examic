//! Exporting a recording's transcript, bookmarks and photos.
//!
//! [`timeline`] merges the three into one chronological list. Markdown is
//! rendered here; the app lays the same timeline out as PDF itself.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use examic_db::{Bookmark, Photo, Recording, TranscriptSegment};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Speech {
        start_ms: i64,
        end_ms: i64,
        text: String,
        translation: Option<String>,
    },
    Bookmark {
        timestamp_ms: i64,
        note: Option<String>,
    },
    Photo {
        timestamp_ms: i64,
        /// Relative to the app's data directory.
        file: String,
    },
}

impl Entry {
    pub fn time_ms(&self) -> i64 {
        match self {
            Entry::Speech { start_ms, .. } => *start_ms,
            Entry::Bookmark { timestamp_ms, .. } | Entry::Photo { timestamp_ms, .. } => {
                *timestamp_ms
            }
        }
    }

    /// At the same moment a bookmark comes first, then a photo, then speech:
    /// marks introduce what is said.
    fn rank(&self) -> u8 {
        match self {
            Entry::Bookmark { .. } => 0,
            Entry::Photo { .. } => 1,
            Entry::Speech { .. } => 2,
        }
    }
}

/// Everything on a recording's timeline, in order.
pub fn timeline(
    transcript: &[TranscriptSegment],
    bookmarks: &[Bookmark],
    photos: &[Photo],
    include_translation: bool,
) -> Vec<Entry> {
    let mut entries: Vec<Entry> = transcript
        .iter()
        .map(|s| Entry::Speech {
            start_ms: s.start_ms,
            end_ms: s.end_ms,
            text: s.text_ru.clone(),
            translation: s.text_en.clone().filter(|_| include_translation),
        })
        .chain(bookmarks.iter().map(|b| Entry::Bookmark {
            timestamp_ms: b.timestamp_ms,
            note: b.note.clone(),
        }))
        .chain(photos.iter().map(|p| Entry::Photo {
            timestamp_ms: p.timestamp_ms,
            file: p.file.clone(),
        }))
        .collect();
    // Stable, so segments keep their order among themselves.
    entries.sort_by_key(|e| (e.time_ms(), e.rank()));
    entries
}

/// `m:ss`, or `h:mm:ss` from an hour on: the player's clock format.
pub fn clock(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    let (h, m, s) = (s / 3600, s % 3600 / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// The name a photo gets inside an export archive.
pub fn photo_name(file: &str) -> String {
    Path::new(file).file_name().map_or_else(
        || file.replace(['/', '\\'], "_"),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// Text that Markdown would otherwise read as formatting at a line start.
fn escape_line(text: &str) -> String {
    let text = text.replace('\n', " ");
    match text.chars().next() {
        Some('#' | '>' | '-' | '+' | '*' | '|') => format!("\\{text}"),
        _ => text,
    }
}

/// The recording as a Markdown document. `date` is shown as given (the app
/// formats it in the user's locale); photos are linked as `photos/<name>`.
pub fn markdown(recording: &Recording, date: &str, entries: &[Entry]) -> String {
    let mut md = String::new();
    md.push_str(&format!("# {}\n\n", escape_line(recording.title.trim())));
    md.push_str(&format!("- **Date:** {date}\n"));
    md.push_str(&format!(
        "- **Duration:** {}\n",
        clock(recording.duration_ms)
    ));
    if !entries.iter().any(|e| matches!(e, Entry::Speech { .. })) {
        md.push_str("\n_No transcript._\n");
    }
    for entry in entries {
        md.push('\n');
        match entry {
            Entry::Speech {
                start_ms,
                text,
                translation,
                ..
            } => {
                md.push_str(&format!(
                    "**[{}]** {}\n",
                    clock(*start_ms),
                    escape_line(text)
                ));
                if let Some(en) = translation {
                    md.push_str(&format!("> EN: {}\n", en.replace('\n', " ")));
                }
            }
            Entry::Bookmark { timestamp_ms, note } => {
                let note = note
                    .as_deref()
                    .map_or_else(|| "Highlight".to_owned(), escape_line);
                md.push_str(&format!("🔖 **[{}]** {note}\n", clock(*timestamp_ms)));
            }
            Entry::Photo { timestamp_ms, file } => {
                md.push_str(&format!(
                    "![{}](photos/{})\n",
                    clock(*timestamp_ms),
                    photo_name(file)
                ));
            }
        }
    }
    md
}

/// What [`write_export`] produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    /// A plain `.md` file.
    Markdown,
    /// A `.zip` with `recording.md` and a `photos/` folder.
    Zip,
}

/// Writes `markdown` to `dest`: as a plain file, or zipped together with the
/// photos (`(name in archive, file on disk)`) when there are any.
pub fn write_export(
    markdown: &str,
    photos: &[(String, PathBuf)],
    dest: &Path,
) -> io::Result<ExportFormat> {
    let mut out = BufWriter::new(File::create(dest)?);
    if photos.is_empty() {
        out.write_all(markdown.as_bytes())?;
        out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        return Ok(ExportFormat::Markdown);
    }
    let mut zip = zip::ZipWriter::new(out);
    let text = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    // JPEGs are already compressed.
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zip.start_file("recording.md", text)
        .map_err(io::Error::other)?;
    zip.write_all(markdown.as_bytes())?;
    for (name, path) in photos {
        match File::open(path) {
            Ok(mut photo) => {
                zip.start_file(format!("photos/{name}"), stored)
                    .map_err(io::Error::other)?;
                io::copy(&mut photo, &mut zip)?;
            }
            // A photo file gone missing should not sink the whole export.
            Err(e) => log::warn!("photo {} left out of the export: {e}", path.display()),
        }
    }
    let out = zip.finish().map_err(io::Error::other)?;
    out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    Ok(ExportFormat::Zip)
}

#[cfg(test)]
mod tests {
    use super::*;
    use examic_db::{RecordingStatus, TranscribeMode, TranscriptStatus};

    fn recording() -> Recording {
        Recording {
            id: "r".into(),
            title: "Лекция 3".into(),
            created_at_ms: 0,
            duration_ms: 3_725_000,
            sample_rate: 48_000,
            audio_file: "r.opus".into(),
            size_bytes: 0,
            status: RecordingStatus::Ready,
            bookmark_count: 1,
            transcribe_mode: TranscribeMode::After,
            translate: true,
            transcript_status: TranscriptStatus::Done,
            photo_count: 1,
        }
    }

    fn segment(start_ms: i64, ru: &str, en: Option<&str>) -> TranscriptSegment {
        TranscriptSegment {
            start_ms,
            end_ms: start_ms + 1_000,
            text_ru: ru.into(),
            text_en: en.map(Into::into),
        }
    }

    fn sample_timeline(include_translation: bool) -> Vec<Entry> {
        timeline(
            &[
                segment(61_000, "Откройте учебник.", Some("Open the textbook.")),
                segment(1_000, "Добрый день.", None),
            ],
            &[
                Bookmark {
                    id: 1,
                    recording_id: "r".into(),
                    timestamp_ms: 61_000,
                    note: Some("# важно".into()),
                },
                Bookmark {
                    id: 2,
                    recording_id: "r".into(),
                    timestamp_ms: 3_700_000,
                    note: None,
                },
            ],
            &[Photo {
                id: 1,
                recording_id: "r".into(),
                timestamp_ms: 61_000,
                file: "photos/r/board.jpg".into(),
            }],
            include_translation,
        )
    }

    #[test]
    fn merges_everything_in_time_order() {
        let t = sample_timeline(true);
        let times: Vec<i64> = t.iter().map(Entry::time_ms).collect();
        assert_eq!(times, [1_000, 61_000, 61_000, 61_000, 3_700_000]);
        assert!(matches!(t[1], Entry::Bookmark { .. }));
        assert!(matches!(t[2], Entry::Photo { .. }));
        assert!(matches!(
            &t[3],
            Entry::Speech {
                translation: Some(_),
                ..
            }
        ));
        assert!(sample_timeline(false).iter().all(|e| !matches!(
            e,
            Entry::Speech {
                translation: Some(_),
                ..
            }
        )));
    }

    #[test]
    fn markdown_snapshot() {
        let md = markdown(&recording(), "13 Nov 2026, 14:05", &sample_timeline(true));
        assert_eq!(
            md,
            "# Лекция 3\n\n\
             - **Date:** 13 Nov 2026, 14:05\n\
             - **Duration:** 1:02:05\n\
             \n**[0:01]** Добрый день.\n\
             \n🔖 **[1:01]** \\# важно\n\
             \n![1:01](photos/board.jpg)\n\
             \n**[1:01]** Откройте учебник.\n> EN: Open the textbook.\n\
             \n🔖 **[1:01:40]** Highlight\n"
        );
        let empty = markdown(&recording(), "today", &[]);
        assert!(empty.contains("_No transcript._"));
    }

    #[test]
    fn exports_plain_markdown_or_a_zip_with_photos() {
        let dir = std::env::temp_dir().join(format!("examic-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let md_path = dir.join("a.md");
        assert_eq!(
            write_export("# hi\n", &[], &md_path).unwrap(),
            ExportFormat::Markdown
        );
        assert_eq!(std::fs::read_to_string(&md_path).unwrap(), "# hi\n");

        let photo = dir.join("board.jpg");
        std::fs::write(&photo, b"\xFF\xD8jpeg").unwrap();
        let zip_path = dir.join("a.zip");
        let photos = [
            ("board.jpg".to_owned(), photo),
            ("gone.jpg".to_owned(), dir.join("gone.jpg")),
        ];
        assert_eq!(
            write_export("# hi\n", &photos, &zip_path).unwrap(),
            ExportFormat::Zip
        );
        let mut archive = zip::ZipArchive::new(File::open(&zip_path).unwrap()).unwrap();
        let names: Vec<String> = archive.file_names().map(String::from).collect();
        assert_eq!(names, ["recording.md", "photos/board.jpg"]);
        let mut md = String::new();
        io::Read::read_to_string(&mut archive.by_name("recording.md").unwrap(), &mut md).unwrap();
        assert_eq!(md, "# hi\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clock_matches_the_player() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(59_999), "0:59");
        assert_eq!(clock(3_600_000), "1:00:00");
        assert_eq!(clock(-5), "0:00");
    }
}
