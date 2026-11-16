//! The library database: which recordings exist, their bookmarks, where
//! speech occurs in them, their transcripts and the photos taken during them.
//!
//! A plain SQLite file in the app's private storage.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, Row, params};

/// One entry per schema version; entry `n` upgrades version `n` to `n + 1`.
/// Shipped entries are never edited, only appended to.
const MIGRATIONS: &[&str] = &[
    "
    CREATE TABLE recordings (
        id            TEXT PRIMARY KEY,
        title         TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL,
        duration_ms   INTEGER NOT NULL DEFAULT 0,
        sample_rate   INTEGER NOT NULL,
        audio_file    TEXT NOT NULL,
        size_bytes    INTEGER NOT NULL DEFAULT 0,
        status        TEXT NOT NULL CHECK (status IN ('RECORDING', 'READY', 'RECOVERED'))
    ) STRICT;
    CREATE INDEX recordings_by_date ON recordings (created_at_ms DESC);

    CREATE TABLE bookmarks (
        id           INTEGER PRIMARY KEY,
        recording_id TEXT NOT NULL REFERENCES recordings (id) ON DELETE CASCADE,
        timestamp_ms INTEGER NOT NULL,
        note         TEXT
    ) STRICT;
    CREATE INDEX bookmarks_by_recording ON bookmarks (recording_id, timestamp_ms);

    CREATE TABLE speech_intervals (
        recording_id TEXT NOT NULL REFERENCES recordings (id) ON DELETE CASCADE,
        start_ms     INTEGER NOT NULL,
        end_ms       INTEGER NOT NULL,
        PRIMARY KEY (recording_id, start_ms)
    ) STRICT, WITHOUT ROWID;
",
    "
    ALTER TABLE recordings ADD COLUMN transcribe_mode TEXT NOT NULL DEFAULT 'OFF'
        CHECK (transcribe_mode IN ('OFF', 'LIVE', 'AFTER'));
    ALTER TABLE recordings ADD COLUMN translate INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE recordings ADD COLUMN transcript_status TEXT NOT NULL DEFAULT 'NONE'
        CHECK (transcript_status IN ('NONE', 'PENDING', 'RUNNING', 'DONE', 'FAILED'));

    CREATE TABLE transcript_segments (
        recording_id TEXT NOT NULL REFERENCES recordings (id) ON DELETE CASCADE,
        start_ms     INTEGER NOT NULL,
        end_ms       INTEGER NOT NULL,
        text_ru      TEXT NOT NULL,
        text_en      TEXT,
        PRIMARY KEY (recording_id, start_ms)
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE photos (
        id           INTEGER PRIMARY KEY,
        recording_id TEXT NOT NULL REFERENCES recordings (id) ON DELETE CASCADE,
        timestamp_ms INTEGER NOT NULL,
        file         TEXT NOT NULL
    ) STRICT;
    CREATE INDEX photos_by_recording ON photos (recording_id, timestamp_ms);
",
];

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("database was written by a newer version of the app (schema {0})")]
    TooNew(i64),
    #[error("recording not found")]
    NotFound,
}

pub type Result<T> = std::result::Result<T, DbError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingStatus {
    /// Capture is in progress, or the app died before it could finish.
    Recording,
    Ready,
    /// Salvaged after an interrupted capture; the end may be missing.
    Recovered,
}

impl RecordingStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Recording => "RECORDING",
            Self::Ready => "READY",
            Self::Recovered => "RECOVERED",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "READY" => Self::Ready,
            "RECOVERED" => Self::Recovered,
            _ => Self::Recording,
        }
    }
}

/// When a recording is transcribed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscribeMode {
    Off,
    /// While recording, one speech segment at a time.
    Live,
    /// In one pass once the recording has stopped.
    After,
}

impl TranscribeMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Off => "OFF",
            Self::Live => "LIVE",
            Self::After => "AFTER",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "LIVE" => Self::Live,
            "AFTER" => Self::After,
            _ => Self::Off,
        }
    }
}

/// Where a recording's transcript stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptStatus {
    /// Never transcribed.
    None,
    /// Queued behind other work.
    Pending,
    Running,
    Done,
    /// Stopped by an error or by the app dying; can be run again.
    Failed,
}

impl TranscriptStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "NONE",
            Self::Pending => "PENDING",
            Self::Running => "RUNNING",
            Self::Done => "DONE",
            Self::Failed => "FAILED",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "PENDING" => Self::Pending,
            "RUNNING" => Self::Running,
            "DONE" => Self::Done,
            "FAILED" => Self::Failed,
            _ => Self::None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recording {
    pub id: String,
    pub title: String,
    pub created_at_ms: i64,
    pub duration_ms: i64,
    pub sample_rate: u32,
    /// File name inside the recordings directory. Relative, so the library
    /// survives the app's data directory moving.
    pub audio_file: String,
    pub size_bytes: i64,
    pub status: RecordingStatus,
    pub bookmark_count: u32,
    pub transcribe_mode: TranscribeMode,
    /// Whether transcribed text is also translated into English.
    pub translate: bool,
    pub transcript_status: TranscriptStatus,
    pub photo_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bookmark {
    pub id: i64,
    pub recording_id: String,
    pub timestamp_ms: i64,
    pub note: Option<String>,
}

/// One stretch of transcribed speech.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptSegment {
    pub start_ms: i64,
    pub end_ms: i64,
    pub text_ru: String,
    pub text_en: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Photo {
    pub id: i64,
    pub recording_id: String,
    pub timestamp_ms: i64,
    /// Path relative to the app's data directory.
    pub file: String,
}

const RECORDING_COLUMNS: &str = "
    id, title, created_at_ms, duration_ms, sample_rate, audio_file, size_bytes, status,
    (SELECT COUNT(*) FROM bookmarks WHERE recording_id = recordings.id),
    transcribe_mode, translate, transcript_status,
    (SELECT COUNT(*) FROM photos WHERE recording_id = recordings.id)";

fn recording_from_row(row: &Row) -> rusqlite::Result<Recording> {
    Ok(Recording {
        id: row.get(0)?,
        title: row.get(1)?,
        created_at_ms: row.get(2)?,
        duration_ms: row.get(3)?,
        sample_rate: row.get(4)?,
        audio_file: row.get(5)?,
        size_bytes: row.get(6)?,
        status: RecordingStatus::parse(&row.get::<_, String>(7)?),
        bookmark_count: row.get(8)?,
        transcribe_mode: TranscribeMode::parse(&row.get::<_, String>(9)?),
        translate: row.get(10)?,
        transcript_status: TranscriptStatus::parse(&row.get::<_, String>(11)?),
        photo_count: row.get(12)?,
    })
}

pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    /// Opens (creating if needed) the database at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        Self::initialise(Connection::open(path)?)
    }

    /// A private in-memory database, for tests.
    pub fn open_in_memory() -> Result<Self> {
        Self::initialise(Connection::open_in_memory()?)
    }

    fn initialise(mut conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        // A panic while holding the lock cannot leave SQLite inconsistent:
        // every multi-statement change runs in a transaction.
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_recording(
        &self,
        id: &str,
        title: &str,
        created_at_ms: i64,
        sample_rate: u32,
        audio_file: &str,
        transcribe_mode: TranscribeMode,
        translate: bool,
    ) -> Result<()> {
        let transcript_status = match transcribe_mode {
            TranscribeMode::Off => TranscriptStatus::None,
            TranscribeMode::Live => TranscriptStatus::Running,
            TranscribeMode::After => TranscriptStatus::Pending,
        };
        self.conn().execute(
            "INSERT INTO recordings (id, title, created_at_ms, sample_rate, audio_file, status,
                                     transcribe_mode, translate, transcript_status)
             VALUES (?1, ?2, ?3, ?4, ?5, 'RECORDING', ?6, ?7, ?8)",
            params![
                id,
                title,
                created_at_ms,
                sample_rate,
                audio_file,
                transcribe_mode.as_str(),
                translate,
                transcript_status.as_str()
            ],
        )?;
        Ok(())
    }

    /// Records the outcome of a capture and where speech was found in it.
    pub fn finish_recording(
        &self,
        id: &str,
        status: RecordingStatus,
        duration_ms: i64,
        size_bytes: i64,
        speech: &[(i64, i64)],
    ) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let updated = tx.execute(
            "UPDATE recordings SET status = ?2, duration_ms = ?3, size_bytes = ?4 WHERE id = ?1",
            params![id, status.as_str(), duration_ms, size_bytes],
        )?;
        if updated == 0 {
            return Err(DbError::NotFound);
        }
        tx.execute("DELETE FROM speech_intervals WHERE recording_id = ?1", [id])?;
        {
            let mut insert = tx.prepare(
                "INSERT OR REPLACE INTO speech_intervals (recording_id, start_ms, end_ms)
                 VALUES (?1, ?2, ?3)",
            )?;
            for &(start_ms, end_ms) in speech {
                insert.execute(params![id, start_ms, end_ms])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn recording(&self, id: &str) -> Result<Recording> {
        self.conn()
            .query_row(
                &format!("SELECT {RECORDING_COLUMNS} FROM recordings WHERE id = ?1"),
                [id],
                recording_from_row,
            )
            .optional()?
            .ok_or(DbError::NotFound)
    }

    /// Every recording, newest first.
    pub fn recordings(&self) -> Result<Vec<Recording>> {
        self.select_recordings("ORDER BY created_at_ms DESC, id DESC")
    }

    /// Recordings whose capture never finished.
    pub fn unfinished_recordings(&self) -> Result<Vec<Recording>> {
        self.select_recordings("WHERE status = 'RECORDING'")
    }

    fn select_recordings(&self, clause: &str) -> Result<Vec<Recording>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {RECORDING_COLUMNS} FROM recordings {clause}"
        ))?;
        let rows = stmt.query_map([], recording_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn rename_recording(&self, id: &str, title: &str) -> Result<()> {
        let updated = self.conn().execute(
            "UPDATE recordings SET title = ?2 WHERE id = ?1",
            params![id, title],
        )?;
        if updated == 0 {
            return Err(DbError::NotFound);
        }
        Ok(())
    }

    /// Deletes a recording with its bookmarks and intervals and returns the
    /// name of its audio file, which the caller removes from disk.
    pub fn delete_recording(&self, id: &str) -> Result<String> {
        self.conn()
            .query_row(
                "DELETE FROM recordings WHERE id = ?1 RETURNING audio_file",
                [id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(DbError::NotFound)
    }

    pub fn add_bookmark(
        &self,
        recording_id: &str,
        timestamp_ms: i64,
        note: Option<&str>,
    ) -> Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO bookmarks (recording_id, timestamp_ms, note) VALUES (?1, ?2, ?3)",
            params![recording_id, timestamp_ms, note],
        )
        .map_err(not_found_on_constraint)?;
        Ok(conn.last_insert_rowid())
    }

    /// Bookmarks of a recording in timeline order.
    pub fn bookmarks(&self, recording_id: &str) -> Result<Vec<Bookmark>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, recording_id, timestamp_ms, note FROM bookmarks
             WHERE recording_id = ?1 ORDER BY timestamp_ms, id",
        )?;
        let rows = stmt.query_map([recording_id], |row| {
            Ok(Bookmark {
                id: row.get(0)?,
                recording_id: row.get(1)?,
                timestamp_ms: row.get(2)?,
                note: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn delete_bookmark(&self, id: i64) -> Result<()> {
        self.conn()
            .execute("DELETE FROM bookmarks WHERE id = ?1", [id])?;
        Ok(())
    }

    /// `(start_ms, end_ms)` of each stretch of speech, in timeline order.
    pub fn speech_intervals(&self, recording_id: &str) -> Result<Vec<(i64, i64)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT start_ms, end_ms FROM speech_intervals
             WHERE recording_id = ?1 ORDER BY start_ms",
        )?;
        let rows = stmt.query_map([recording_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_transcript_status(&self, id: &str, status: TranscriptStatus) -> Result<()> {
        let updated = self.conn().execute(
            "UPDATE recordings SET transcript_status = ?2 WHERE id = ?1",
            params![id, status.as_str()],
        )?;
        if updated == 0 {
            return Err(DbError::NotFound);
        }
        Ok(())
    }

    /// Sets the translation option and returns the recording's new state.
    pub fn set_translate(&self, id: &str, translate: bool) -> Result<()> {
        let updated = self.conn().execute(
            "UPDATE recordings SET translate = ?2 WHERE id = ?1",
            params![id, translate],
        )?;
        if updated == 0 {
            return Err(DbError::NotFound);
        }
        Ok(())
    }

    /// Transcription work that a previous run left unfinished becomes
    /// `Failed`, so it can be started again. Returns how many were reset.
    pub fn fail_unfinished_transcripts(&self) -> Result<usize> {
        Ok(self.conn().execute(
            "UPDATE recordings SET transcript_status = 'FAILED'
             WHERE transcript_status IN ('PENDING', 'RUNNING') AND status != 'RECORDING'",
            [],
        )?)
    }

    /// Removes a recording's transcript before it is transcribed again.
    pub fn clear_transcript(&self, recording_id: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM transcript_segments WHERE recording_id = ?1",
            [recording_id],
        )?;
        Ok(())
    }

    /// Stores segments, replacing any that start at the same moment.
    pub fn add_transcript_segments(
        &self,
        recording_id: &str,
        segments: &[TranscriptSegment],
    ) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        {
            let mut insert = tx.prepare(
                "INSERT OR REPLACE INTO transcript_segments
                     (recording_id, start_ms, end_ms, text_ru, text_en)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for s in segments {
                insert
                    .execute(params![
                        recording_id,
                        s.start_ms,
                        s.end_ms,
                        s.text_ru,
                        s.text_en
                    ])
                    .map_err(not_found_on_constraint)?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn set_segment_translation(
        &self,
        recording_id: &str,
        start_ms: i64,
        text_en: Option<&str>,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE transcript_segments SET text_en = ?3 WHERE recording_id = ?1 AND start_ms = ?2",
            params![recording_id, start_ms, text_en],
        )?;
        Ok(())
    }

    /// A recording's transcript in timeline order.
    pub fn transcript(&self, recording_id: &str) -> Result<Vec<TranscriptSegment>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT start_ms, end_ms, text_ru, text_en FROM transcript_segments
             WHERE recording_id = ?1 ORDER BY start_ms",
        )?;
        let rows = stmt.query_map([recording_id], |row| {
            Ok(TranscriptSegment {
                start_ms: row.get(0)?,
                end_ms: row.get(1)?,
                text_ru: row.get(2)?,
                text_en: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn add_photo(&self, recording_id: &str, timestamp_ms: i64, file: &str) -> Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO photos (recording_id, timestamp_ms, file) VALUES (?1, ?2, ?3)",
            params![recording_id, timestamp_ms, file],
        )
        .map_err(not_found_on_constraint)?;
        Ok(conn.last_insert_rowid())
    }

    /// Photos of a recording in timeline order.
    pub fn photos(&self, recording_id: &str) -> Result<Vec<Photo>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, recording_id, timestamp_ms, file FROM photos
             WHERE recording_id = ?1 ORDER BY timestamp_ms, id",
        )?;
        let rows = stmt.query_map([recording_id], photo_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Deletes a photo and returns it, so the caller can remove its file.
    pub fn delete_photo(&self, id: i64) -> Result<Photo> {
        self.conn()
            .query_row(
                "DELETE FROM photos WHERE id = ?1 RETURNING id, recording_id, timestamp_ms, file",
                [id],
                photo_from_row,
            )
            .optional()?
            .ok_or(DbError::NotFound)
    }
}

fn photo_from_row(row: &Row) -> rusqlite::Result<Photo> {
    Ok(Photo {
        id: row.get(0)?,
        recording_id: row.get(1)?,
        timestamp_ms: row.get(2)?,
        file: row.get(3)?,
    })
}

/// A foreign-key violation on insert means the recording does not exist.
fn not_found_on_constraint(e: rusqlite::Error) -> DbError {
    match e {
        rusqlite::Error::SqliteFailure(f, _)
            if f.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            DbError::NotFound
        }
        other => DbError::Sqlite(other),
    }
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > MIGRATIONS.len() as i64 {
        return Err(DbError::TooNew(version));
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", index as i64 + 1)?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Database {
        Database::open_in_memory().unwrap()
    }

    fn add(db: &Database, id: &str, created_at_ms: i64) {
        db.create_recording(
            id,
            &format!("Title {id}"),
            created_at_ms,
            48_000,
            &format!("{id}.opus"),
            TranscribeMode::Off,
            false,
        )
        .unwrap();
    }

    #[test]
    fn a_new_recording_starts_unfinished() {
        let db = db();
        add(&db, "a", 1_000);
        let rec = db.recording("a").unwrap();
        assert_eq!(rec.title, "Title a");
        assert_eq!(rec.status, RecordingStatus::Recording);
        assert_eq!(
            (rec.duration_ms, rec.size_bytes, rec.bookmark_count),
            (0, 0, 0)
        );
        assert_eq!(rec.sample_rate, 48_000);
        assert_eq!(db.unfinished_recordings().unwrap(), [rec]);
    }

    #[test]
    fn finishing_stores_totals_and_speech() {
        let db = db();
        add(&db, "a", 1_000);
        db.finish_recording(
            "a",
            RecordingStatus::Ready,
            60_000,
            240_000,
            &[(500, 900), (100, 400)],
        )
        .unwrap();
        let rec = db.recording("a").unwrap();
        assert_eq!(rec.status, RecordingStatus::Ready);
        assert_eq!((rec.duration_ms, rec.size_bytes), (60_000, 240_000));
        assert_eq!(db.speech_intervals("a").unwrap(), [(100, 400), (500, 900)]);
        assert!(db.unfinished_recordings().unwrap().is_empty());

        // Finishing again (a recovery re-run) replaces, not duplicates.
        db.finish_recording("a", RecordingStatus::Recovered, 50_000, 1, &[(0, 10)])
            .unwrap();
        assert_eq!(db.speech_intervals("a").unwrap(), [(0, 10)]);
        assert_eq!(
            db.recording("a").unwrap().status,
            RecordingStatus::Recovered
        );
    }

    #[test]
    fn lists_newest_first() {
        let db = db();
        add(&db, "old", 1_000);
        add(&db, "new", 3_000);
        add(&db, "mid", 2_000);
        let ids: Vec<_> = db.recordings().unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["new", "mid", "old"]);
    }

    #[test]
    fn bookmarks_are_ordered_and_counted() {
        let db = db();
        add(&db, "a", 1_000);
        let late = db.add_bookmark("a", 9_000, Some("decision")).unwrap();
        let early = db.add_bookmark("a", 2_000, None).unwrap();
        let marks = db.bookmarks("a").unwrap();
        assert_eq!(
            marks.iter().map(|b| b.id).collect::<Vec<_>>(),
            [early, late]
        );
        assert_eq!(marks[0].note, None);
        assert_eq!(marks[1].note.as_deref(), Some("decision"));
        assert_eq!(db.recording("a").unwrap().bookmark_count, 2);

        db.delete_bookmark(early).unwrap();
        assert_eq!(db.recording("a").unwrap().bookmark_count, 1);
    }

    #[test]
    fn deleting_a_recording_removes_everything_that_hangs_off_it() {
        let db = db();
        add(&db, "a", 1_000);
        add(&db, "b", 2_000);
        db.add_bookmark("a", 1, None).unwrap();
        db.add_bookmark("b", 1, None).unwrap();
        db.finish_recording("a", RecordingStatus::Ready, 1, 1, &[(0, 1)])
            .unwrap();

        assert_eq!(db.delete_recording("a").unwrap(), "a.opus");
        assert!(matches!(db.recording("a"), Err(DbError::NotFound)));
        assert!(db.bookmarks("a").unwrap().is_empty());
        assert!(db.speech_intervals("a").unwrap().is_empty());
        assert_eq!(db.bookmarks("b").unwrap().len(), 1);
    }

    #[test]
    fn missing_recordings_are_reported_not_ignored() {
        let db = db();
        assert!(matches!(db.recording("x"), Err(DbError::NotFound)));
        assert!(matches!(
            db.rename_recording("x", "t"),
            Err(DbError::NotFound)
        ));
        assert!(matches!(db.delete_recording("x"), Err(DbError::NotFound)));
        assert!(matches!(
            db.add_bookmark("x", 0, None),
            Err(DbError::NotFound)
        ));
        assert!(matches!(
            db.finish_recording("x", RecordingStatus::Ready, 0, 0, &[]),
            Err(DbError::NotFound)
        ));
    }

    #[test]
    fn rename_changes_only_the_title() {
        let db = db();
        add(&db, "a", 1_000);
        db.rename_recording("a", "Board meeting").unwrap();
        let rec = db.recording("a").unwrap();
        assert_eq!(
            (rec.title.as_str(), rec.audio_file.as_str()),
            ("Board meeting", "a.opus")
        );
    }

    #[test]
    fn data_survives_reopening_and_migrations_run_once() {
        let dir = std::env::temp_dir().join(format!("examic-db-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("reopen.db");
        let _ = std::fs::remove_file(&path);
        {
            let db = Database::open(&path).unwrap();
            add(&db, "a", 1_000);
        }
        let db = Database::open(&path).unwrap();
        assert_eq!(db.recordings().unwrap().len(), 1);
        drop(db);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn transcription_options_start_the_right_status() {
        let db = db();
        let create = |id: &str, mode| {
            db.create_recording(id, id, 1, 48_000, "x.opus", mode, true)
                .unwrap();
            db.recording(id).unwrap()
        };
        let off = create("off", TranscribeMode::Off);
        let live = create("live", TranscribeMode::Live);
        let after = create("after", TranscribeMode::After);
        assert_eq!(off.transcript_status, TranscriptStatus::None);
        assert_eq!(live.transcript_status, TranscriptStatus::Running);
        assert_eq!(after.transcript_status, TranscriptStatus::Pending);
        assert_eq!(after.transcribe_mode, TranscribeMode::After);
        assert!(after.translate);

        // A crash leaves live/after work unfinished; once the recordings are
        // no longer being captured, it is marked failed so it can be retried.
        for id in ["live", "after"] {
            db.finish_recording(id, RecordingStatus::Recovered, 1, 1, &[])
                .unwrap();
        }
        assert_eq!(db.fail_unfinished_transcripts().unwrap(), 2);
        assert_eq!(
            db.recording("live").unwrap().transcript_status,
            TranscriptStatus::Failed
        );
        assert_eq!(
            db.recording("off").unwrap().transcript_status,
            TranscriptStatus::None
        );
    }

    #[test]
    fn transcript_segments_are_ordered_replaced_and_translated() {
        let db = db();
        add(&db, "a", 1);
        let seg = |start_ms, text: &str| TranscriptSegment {
            start_ms,
            end_ms: start_ms + 500,
            text_ru: text.into(),
            text_en: None,
        };
        db.add_transcript_segments("a", &[seg(2000, "вторая"), seg(0, "первая")])
            .unwrap();
        db.add_transcript_segments("a", &[seg(2000, "вторая, исправленная")])
            .unwrap();
        db.set_segment_translation("a", 0, Some("first")).unwrap();
        let t = db.transcript("a").unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].text_en.as_deref(), Some("first"));
        assert_eq!(t[1].text_ru, "вторая, исправленная");
        assert!(matches!(
            db.add_transcript_segments("missing", &[seg(0, "x")]),
            Err(DbError::NotFound)
        ));

        db.clear_transcript("a").unwrap();
        assert!(db.transcript("a").unwrap().is_empty());
    }

    #[test]
    fn photos_are_ordered_counted_and_go_with_their_recording() {
        let db = db();
        add(&db, "a", 1);
        let late = db.add_photo("a", 9_000, "photos/a/2.jpg").unwrap();
        let early = db.add_photo("a", 1_000, "photos/a/1.jpg").unwrap();
        let photos = db.photos("a").unwrap();
        assert_eq!(
            photos.iter().map(|p| p.id).collect::<Vec<_>>(),
            [early, late]
        );
        assert_eq!(db.recording("a").unwrap().photo_count, 2);
        assert_eq!(db.delete_photo(early).unwrap().file, "photos/a/1.jpg");
        assert!(matches!(db.delete_photo(early), Err(DbError::NotFound)));
        assert!(matches!(
            db.add_photo("missing", 0, "x.jpg"),
            Err(DbError::NotFound)
        ));
        db.delete_recording("a").unwrap();
        assert!(db.photos("a").unwrap().is_empty());
    }

    #[test]
    fn refuses_a_database_from_a_newer_app() {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
        assert!(matches!(
            Database::initialise(conn),
            Err(DbError::TooNew(99))
        ));
    }
}
