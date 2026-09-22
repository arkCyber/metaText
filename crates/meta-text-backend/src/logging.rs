/*!
 * logging.rs
 *
 * A file appender that rotates by size *and* by day.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2026-09-14
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - Bounded log files: a file is rolled once it reaches `rotation_size_mb`
 * - Daily files are kept as before (`{prefix}.YYYY-MM-DD`), with size rolls
 *   appended as `{prefix}.YYYY-MM-DD.001`
 * - Retention: at most `max_files` files survive, oldest first
 * - A write is never split across two files, so a log line cannot be torn
 *
 * # Why this exists
 *
 * `[logging] rotation_size_mb` was accepted, validated and then read by nothing:
 * `tracing_appender`'s rolling appender rotates by time only, so an operator who
 * asked for 10 MB files got unlimited daily ones instead. The project's rule for a
 * configuration key is that it either changes behaviour or is rejected loudly (see
 * `docs/ARCHITECTURE.md`, A14/A42) — and a long-running peer can produce far more
 * than one file a day, so the size limit is the one that matters in practice.
 *
 * # Naming and ordering
 *
 * ```text
 * logs/meta-text.log.2026-09-14        first file of the day
 * logs/meta-text.log.2026-09-14.001    it filled up, this is the continuation
 * logs/meta-text.log.2026-09-14.002    ...
 * ```
 *
 * The index is zero padded to three digits so a plain lexicographic sort of the
 * directory is chronological (`….010` after `….009`). Retention does not rely on
 * that, though: files are ordered by the `(date, index)` they encode, so a fourth
 * digit (an implausibly busy day) still ages out correctly.
 *
 * # Threading
 *
 * `std::io::Write` and nothing else, so the writer composes with
 * [`tracing_appender::non_blocking()`]: the subscriber hands it whole log lines from
 * its own thread and never waits for the disk.
 */

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Name of the file inside `directory` for one `(date, index)` pair.
///
/// `index == 0` is the first file of the day and carries no suffix, which keeps
/// the names a reader already knows from the time based appender.
#[must_use]
fn file_name(prefix: &str, stamp: &str, index: u32) -> String {
    if index == 0 {
        format!("{prefix}.{stamp}")
    } else {
        format!("{prefix}.{stamp}.{index:03}")
    }
}

/// Parse `{prefix}.{stamp}[.{index}]` back into its ordering key.
///
/// Returns `None` for anything this appender did not write, so another program's
/// files in the same directory are never deleted by retention.
#[must_use]
fn parse_file_name(name: &str, prefix: &str) -> Option<(String, u32)> {
    let rest = name.strip_prefix(prefix)?.strip_prefix('.')?;
    // A date is `YYYY-MM-DD`; anything shorter cannot be one of ours.
    if rest.len() < 10 || rest.as_bytes().get(4) != Some(&b'-') {
        return None;
    }
    let (stamp, index) = match rest.split_once('.') {
        Some((stamp, index)) => (stamp, index.parse::<u32>().ok()?),
        None => (rest, 0),
    };
    Some((stamp.to_string(), index))
}

/// The current UTC date, as the appender stamps its files.
#[must_use]
fn utc_stamp() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

/// A log file the appender is currently appending to.
#[derive(Debug)]
struct OpenFile {
    /// The file handle, opened for appending.
    file: File,

    /// The day it belongs to, as `YYYY-MM-DD`.
    stamp: String,

    /// Position within that day (`0` is the first file).
    index: u32,

    /// Bytes written to this file so far, including what was already there when
    /// the process started.
    written: u64,
}

/// A `Write` that rolls its file over when it reaches a size, then a day.
///
/// See the [module documentation](self) for the file naming, the ordering and why
/// the size limit exists at all.
pub struct SizeRotatingWriter {
    /// Directory the files live in.
    directory: PathBuf,

    /// File name prefix (the configured path's file name).
    prefix: String,

    /// Size at which a file is rolled, in bytes. Never `0`.
    max_bytes: u64,

    /// How many files (the active one included) may exist at once. Never `0`.
    max_files: usize,

    /// Reads the current day; a field rather than a call so the rollover paths —
    /// which are otherwise only reachable after midnight — can be tested.
    clock: Box<dyn Fn() -> String + Send + Sync>,

    /// The file currently being appended to, if any.
    open: Option<OpenFile>,
}

impl std::fmt::Debug for SizeRotatingWriter {
    /// Describe the writer without the clock, which has no `Debug` impl.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SizeRotatingWriter")
            .field("directory", &self.directory)
            .field("prefix", &self.prefix)
            .field("max_bytes", &self.max_bytes)
            .field("max_files", &self.max_files)
            .field("active", &self.active_path())
            .finish_non_exhaustive()
    }
}

impl SizeRotatingWriter {
    /// Create a writer that keeps at most `max_files` files of `max_bytes` each.
    ///
    /// `max_bytes` and `max_files` are clamped to at least one: a zero byte limit
    /// would roll on every write and a zero file limit would delete the file being
    /// written. The configuration validator rejects both, and the clamp keeps a
    /// programmatic caller from producing an appender that destroys its own output.
    #[must_use]
    pub fn new(
        directory: impl Into<PathBuf>,
        prefix: impl Into<String>,
        max_bytes: u64,
        max_files: usize,
    ) -> Self {
        Self::with_clock(directory, prefix, max_bytes, max_files, Box::new(utc_stamp))
    }

    /// Build a writer with an explicit day source.
    fn with_clock(
        directory: impl Into<PathBuf>,
        prefix: impl Into<String>,
        max_bytes: u64,
        max_files: usize,
        clock: Box<dyn Fn() -> String + Send + Sync>,
    ) -> Self {
        let prefix = prefix.into();
        Self {
            directory: directory.into(),
            prefix: if prefix.is_empty() {
                "meta-text.log".to_string()
            } else {
                prefix
            },
            max_bytes: max_bytes.max(1),
            max_files: max_files.max(1),
            clock,
            open: None,
        }
    }

    /// Convert a `[logging] rotation_size_mb` value into the byte limit.
    ///
    /// `0` becomes one byte rather than "unlimited": the validator rejects `0`, and
    /// a programmatic caller that passes it gets a working appender instead of one
    /// that never rolls.
    #[must_use]
    pub const fn rotation_bytes(megabytes: u64) -> u64 {
        let bytes = megabytes.saturating_mul(1024 * 1024);
        if bytes == 0 {
            1
        } else {
            bytes
        }
    }

    /// The size a file is rolled at, in bytes.
    #[must_use]
    pub const fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// How many files the appender keeps, the active one included.
    #[must_use]
    pub const fn max_files(&self) -> usize {
        self.max_files
    }

    /// The directory the files are written to.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// The file being appended to, once something has been written.
    #[must_use]
    pub fn active_path(&self) -> Option<PathBuf> {
        self.open
            .as_ref()
            .map(|open| self.path_for(&open.stamp, open.index))
    }

    /// Open the current file now, without writing anything to it.
    ///
    /// [`tracing_appender::non_blocking()`] reports I/O failures on its own thread, so
    /// a log directory that cannot be created would otherwise fail silently and lose
    /// every line. Calling this at startup turns that into an error the process can
    /// report while it still has somewhere to report it.
    ///
    /// # Errors
    ///
    /// Returns the underlying I/O error when the file cannot be opened or created.
    pub fn open(&mut self) -> io::Result<()> {
        self.prepare(0)
    }

    /// The path of one `(date, index)` file.
    fn path_for(&self, stamp: &str, index: u32) -> PathBuf {
        self.directory.join(file_name(&self.prefix, stamp, index))
    }

    /// Every file in the directory this appender wrote, as `(path, date, index)`.
    fn scan(&self) -> Vec<(PathBuf, String, u32)> {
        let Ok(entries) = fs::read_dir(&self.directory) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let name = name.to_str()?;
                let (stamp, index) = parse_file_name(name, &self.prefix)?;
                Some((entry.path(), stamp, index))
            })
            .collect()
    }

    /// The next free index for `stamp`: `0` when the day has no file yet, otherwise
    /// one past the highest index that exists.
    ///
    /// Starting a new day at `0` is what keeps the familiar unsuffixed name for the
    /// first file of that day (`.001` would be the continuation of a file that never
    /// existed). Reusing the highest index across a restart is what stops today's
    /// rolls from overwriting a file an earlier run of the same day left behind.
    fn next_index(&self, stamp: &str) -> u32 {
        self.scan()
            .into_iter()
            .filter(|(_, existing, _)| existing == stamp)
            .map(|(_, _, index)| index)
            .max()
            .map_or(0, |highest| highest.saturating_add(1))
    }

    /// Open `(stamp, index)` for appending, remembering what it already holds.
    ///
    /// The directory is created if it is missing, which is what the previous
    /// appender did too: the shipped configuration points at `logs/meta-text.log`,
    /// and a fresh checkout (or a container) has no `logs/` yet — failing there
    /// would make the default configuration unusable.
    fn open_file(&mut self, stamp: &str, index: u32) -> io::Result<()> {
        fs::create_dir_all(&self.directory)?;
        let path = self.path_for(stamp, index);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        // A file left behind by an earlier run of the day already counts towards
        // the limit; starting the count at zero would double the configured size.
        let written = file.metadata().map_or(0, |meta| meta.len());
        self.open = Some(OpenFile {
            file,
            stamp: stamp.to_string(),
            index,
            written,
        });
        Ok(())
    }

    /// Decide whether the next write fits, rolling the file when it does not.
    ///
    /// Rolling happens *before* the write, so a log line is never split across two
    /// files. A line larger than the whole limit is written anyway, inside one
    /// file: refusing it would lose the very message that explains why the limit is
    /// being hit, and rolling first would only move the same oversized line into a
    /// fresh file.
    ///
    /// The size check always runs on the file that is about to be appended to, so
    /// the two "open something first" branches fall through instead of returning: a
    /// file left behind by an earlier run of the day can already be at the limit.
    fn prepare(&mut self, pending: u64) -> io::Result<()> {
        let stamp = (self.clock)();

        match self.open.as_ref() {
            None => {
                self.open_file(&stamp, 0)?;
                self.prune();
            }
            Some(open) if open.stamp != stamp => {
                let next = self.next_index(&stamp);
                self.open_file(&stamp, next)?;
                self.prune();
            }
            Some(_) => {}
        }

        if let Some(open) = self.open.as_ref() {
            let roll = open.written > 0 && open.written.saturating_add(pending) > self.max_bytes;
            if roll {
                let (stamp, next) = (open.stamp.clone(), open.index.saturating_add(1));
                self.open_file(&stamp, next)?;
                self.prune();
            }
        }
        Ok(())
    }

    /// Delete the oldest files until at most `max_files` (active one included) remain.
    ///
    /// Best effort: a file that cannot be removed is left in place rather than
    /// failing the write. Reporting it through `tracing` is not an option — this is
    /// the appender `tracing` writes to — so the failure is silent by design.
    fn prune(&self) {
        let active = self
            .open
            .as_ref()
            .map(|open| self.path_for(&open.stamp, open.index));

        let mut files = self.scan();
        // `(date, index)` is exactly the order the files were created in, which is
        // why the index is compared as a number and not as text.
        files.sort_by(|a, b| (&a.1, a.2).cmp(&(&b.1, b.2)));

        let excess = files.len().saturating_sub(self.max_files);
        for (path, _, _) in files.into_iter().take(excess) {
            if active.as_deref() == Some(path.as_path()) {
                continue;
            }
            let _ = fs::remove_file(path);
        }
    }
}

/// Build the file appender for a `[logging]` section.
///
/// Returns the writer and the directory it was configured to write into, so a
/// caller can name that directory when opening fails. The mapping lives here rather
/// than in the binary so the configuration actually reaching the appender is
/// testable: `rotation_size_mb` used to be read by nothing at all, which is the
/// failure mode this replaces.
#[must_use]
pub fn file_writer(config: &crate::config::LoggingConfig) -> (SizeRotatingWriter, PathBuf) {
    // The configured path is `{directory}/{prefix}`: the appender derives
    // `{prefix}.{date}[.{n}]` from the two parts itself.
    let path = Path::new(config.file_path.trim());
    let directory = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let prefix = path.file_name().map_or_else(
        || "meta-text.log".to_string(),
        |name| name.to_string_lossy().into_owned(),
    );

    let writer = SizeRotatingWriter::new(
        &directory,
        prefix,
        SizeRotatingWriter::rotation_bytes(config.rotation_size_mb),
        config.max_files,
    );
    (writer, directory)
}

impl Write for SizeRotatingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.prepare(buf.len() as u64)?;

        let Some(open) = self.open.as_mut() else {
            return Err(io::Error::other(
                "the log appender has no open file after rolling",
            ));
        };
        open.file.write_all(buf)?;
        open.written = open.written.saturating_add(buf.len() as u64);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.open.as_mut() {
            Some(open) => open.file.flush(),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A writer whose "current day" the test can move forward.
    fn scripted_writer(
        dir: &Path,
        max_bytes: u64,
        max_files: usize,
        day: Arc<Mutex<String>>,
    ) -> SizeRotatingWriter {
        SizeRotatingWriter::with_clock(
            dir,
            "meta-text.log",
            max_bytes,
            max_files,
            Box::new(move || day.lock().expect("day").clone()),
        )
    }

    /// Every file the appender wrote, ordered the way retention orders them.
    fn files(dir: &Path) -> Vec<String> {
        let mut entries: Vec<(String, u32, String)> = fs::read_dir(dir)
            .expect("read dir")
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let name = name.to_str()?.to_string();
                let (stamp, index) = parse_file_name(&name, "meta-text.log")?;
                Some((stamp, index, name))
            })
            .collect();
        // Ordered by `(date, index)`, which is the order the files were created
        // in — the name alone would only sort that way thanks to the padding.
        entries.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
        entries.into_iter().map(|(_, _, name)| name).collect()
    }

    /// A path with symlinks resolved, so a temporary directory compares equal to
    /// the path the OS reports for it (`/var` vs `/private/var` on macOS).
    fn walk(path: &Path) -> PathBuf {
        fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }

    /// A file name is the prefix, the day, and a zero padded roll index.
    #[test]
    fn test_file_names_are_ordered_and_parsed() {
        assert_eq!(file_name("app.log", "2026-09-14", 0), "app.log.2026-09-14");
        assert_eq!(
            file_name("app.log", "2026-09-14", 7),
            "app.log.2026-09-14.007"
        );
        // The padding is what makes a plain directory listing chronological.
        assert!(file_name("app.log", "2026-09-14", 10) > file_name("app.log", "2026-09-14", 9));

        assert_eq!(
            parse_file_name("app.log.2026-09-14", "app.log"),
            Some(("2026-09-14".to_string(), 0))
        );
        assert_eq!(
            parse_file_name("app.log.2026-09-14.012", "app.log"),
            Some(("2026-09-14".to_string(), 12))
        );

        // Anything that is not ours is not touched — retention deletes files.
        for foreign in [
            "other.log.2026-09-14",
            "app.log",
            "app.log.not-a-date",
            "app.log.2026-09-14.x",
        ] {
            assert_eq!(parse_file_name(foreign, "app.log"), None, "{foreign}");
        }
    }

    /// The configured megabytes become a byte limit, and `0` never means "roll not".
    #[test]
    fn test_rotation_size_conversion() {
        assert_eq!(SizeRotatingWriter::rotation_bytes(1), 1024 * 1024);
        assert_eq!(SizeRotatingWriter::rotation_bytes(10), 10 * 1024 * 1024);
        assert_eq!(SizeRotatingWriter::rotation_bytes(0), 1);
        // A preposterous value saturates instead of wrapping into a tiny file.
        assert_eq!(SizeRotatingWriter::rotation_bytes(u64::MAX), u64::MAX);
    }

    /// Writes stay in one file until the limit, then continue in the next one.
    #[test]
    fn test_rolling_at_the_size_limit_keeps_a_line_whole() {
        let dir = tempfile::tempdir().expect("tempdir");
        let day = Arc::new(Mutex::new("2026-09-14".to_string()));
        let mut writer = scripted_writer(dir.path(), 20, 10, Arc::clone(&day));

        // Three 8 byte lines: the third would pass 20 bytes, so it opens `.001`.
        for line in ["line-001", "line-002", "line-003"] {
            writer.write_all(line.as_bytes()).expect("write");
        }
        writer.flush().expect("flush");

        assert_eq!(
            files(dir.path()),
            vec!["meta-text.log.2026-09-14", "meta-text.log.2026-09-14.001"]
        );
        // No line was torn in half: the first two are in the first file and the
        // third starts the second.
        assert_eq!(
            fs::read_to_string(dir.path().join("meta-text.log.2026-09-14")).expect("read"),
            "line-001line-002"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("meta-text.log.2026-09-14.001")).expect("read"),
            "line-003"
        );
    }

    /// A line bigger than the whole limit is still written, inside one file.
    #[test]
    fn test_an_oversized_line_is_written_whole() {
        let dir = tempfile::tempdir().expect("tempdir");
        let day = Arc::new(Mutex::new("2026-09-14".to_string()));
        let mut writer = scripted_writer(dir.path(), 8, 10, day);

        // 40 bytes against an 8 byte limit: written, not dropped, and rolling
        // first would not have helped.
        writer.write_all("x".repeat(40).as_bytes()).expect("write");
        writer.flush().expect("flush");
        assert_eq!(
            files(dir.path()),
            vec!["meta-text.log.2026-09-14"],
            "an oversized line must not roll on its own"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("meta-text.log.2026-09-14"))
                .expect("read")
                .len(),
            40
        );

        // The next line rolls, so the limit is still enforced afterwards.
        writer.write_all(b"tail").expect("write");
        writer.flush().expect("flush");
        assert_eq!(
            files(dir.path()),
            vec!["meta-text.log.2026-09-14", "meta-text.log.2026-09-14.001"]
        );
    }

    /// Only `max_files` files survive, and the survivors are the newest ones.
    #[test]
    fn test_retention_keeps_the_newest_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let day = Arc::new(Mutex::new("2026-09-14".to_string()));
        // 4 byte files with room for two: five writes leave the last two lines.
        let mut writer = scripted_writer(dir.path(), 4, 2, day);
        for line in ["aaa", "bbb", "ccc", "ddd", "eee"] {
            writer.write_all(line.as_bytes()).expect("write");
        }
        writer.flush().expect("flush");

        let names = files(dir.path());
        assert_eq!(names.len(), 2, "{names:?}");
        assert_eq!(
            names,
            vec![
                "meta-text.log.2026-09-14.003",
                "meta-text.log.2026-09-14.004"
            ]
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("meta-text.log.2026-09-14.004")).expect("read"),
            "eee"
        );
    }

    /// A new day starts a new file even when the current one is far from full.
    #[test]
    fn test_a_new_day_starts_a_new_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let day = Arc::new(Mutex::new("2026-09-14".to_string()));
        let mut writer = scripted_writer(dir.path(), 1024, 10, Arc::clone(&day));

        writer.write_all(b"before midnight").expect("write");
        *day.lock().expect("day") = "2026-09-15".to_string();
        writer.write_all(b"after midnight").expect("write");
        writer.flush().expect("flush");

        assert_eq!(
            files(dir.path()),
            vec!["meta-text.log.2026-09-14", "meta-text.log.2026-09-15"]
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("meta-text.log.2026-09-15")).expect("read"),
            "after midnight"
        );
    }

    /// Restarting continues in the day's file and counts what is already there.
    #[test]
    fn test_a_restart_continues_in_the_same_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let day = Arc::new(Mutex::new("2026-09-14".to_string()));

        let mut first = scripted_writer(dir.path(), 20, 10, Arc::clone(&day));
        first.write_all(b"0123456789abcdef").expect("write");
        first.flush().expect("flush");
        drop(first);

        let mut second = scripted_writer(dir.path(), 20, 10, Arc::clone(&day));
        // 16 bytes are already on disk, so these 5 bytes cross the 20 byte limit...
        second.write_all(b"bbbbb").expect("write");
        second.flush().expect("flush");

        // ...which means a restarted process rolls instead of growing the file past
        // the configured size. A writer that started its byte count at zero would
        // have kept appending here (`5 <= 20`), which is the bug this pins.
        assert_eq!(
            files(dir.path()),
            vec!["meta-text.log.2026-09-14", "meta-text.log.2026-09-14.001"]
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("meta-text.log.2026-09-14.001")).expect("read"),
            "bbbbb"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("meta-text.log.2026-09-14")).expect("read"),
            "0123456789abcdef",
            "the first run's bytes must be left untouched"
        );
    }

    /// Other programs' files in the log directory are never deleted.
    #[test]
    fn test_foreign_files_are_left_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let foreign = dir.path().join("other.log.2026-01-01");
        fs::write(&foreign, "not ours").expect("write foreign");

        let day = Arc::new(Mutex::new("2026-09-14".to_string()));
        let mut writer = scripted_writer(dir.path(), 4, 1, day);
        for line in ["aaa", "bbb", "ccc"] {
            writer.write_all(line.as_bytes()).expect("write");
        }
        writer.flush().expect("flush");

        assert!(foreign.exists(), "a foreign file must survive retention");
        assert_eq!(files(dir.path()).len(), 1, "only the newest file is kept");
    }

    /// A missing log directory is created, like the time based appender did.
    #[test]
    fn test_a_missing_directory_is_created() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("logs").join("nested");
        let day = Arc::new(Mutex::new("2026-09-14".to_string()));
        let mut writer = scripted_writer(&nested, 1024, 3, day);

        writer.write_all(b"hi").expect("write");
        writer.flush().expect("flush");

        assert_eq!(files(&nested), vec!["meta-text.log.2026-09-14"]);
    }

    /// The `[logging]` section reaches the appender: size, retention and path.
    ///
    /// This is the assertion the old build could not make at all — the key was
    /// parsed, validated and then read by nothing.
    #[test]
    fn test_the_configuration_reaches_the_appender() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = crate::config::LoggingConfig {
            file_path: dir.path().join("peer.log").to_string_lossy().into_owned(),
            rotation_size_mb: 3,
            max_files: 7,
            ..crate::config::LoggingConfig::default()
        };

        let (mut writer, directory) = file_writer(&config);
        assert_eq!(
            walk(&directory),
            walk(dir.path()),
            "the path's directory is used"
        );
        assert_eq!(writer.max_bytes(), 3 * 1024 * 1024);
        assert_eq!(writer.max_files(), 7);

        // The configured file name is the prefix; the appender adds `.{date}`.
        writer.write_all(b"hello").expect("write");
        writer.flush().expect("flush");
        let active = writer.active_path().expect("an open file");
        assert_eq!(active.parent(), Some(dir.path()));
        let name = active
            .file_name()
            .expect("file name")
            .to_string_lossy()
            .into_owned();
        assert!(
            parse_file_name(&name, "peer.log").is_some(),
            "the open file must be `<configured name>.<date>`: {name}"
        );
        assert_eq!(fs::read_to_string(&active).expect("read"), "hello");
    }
}
