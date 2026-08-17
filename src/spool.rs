// The raw spool: the ground truth behind every filtered result
// (docs/wrap-watch.md §2).
//
// A filtered payload is only safe to trust because the full output it was
// derived from is still on disk and named in the payload — §2.4's
// recoverability contract.  So this module has one job and one failure rule:
// persist a blob, and never make its own trouble the caller's.  An unwritable
// cache dir, a full disk, a directory someone chmod'd to 0000 all degrade to
// "no spool" exactly the way `stats::append_line` degrades to "no log line".
//
// Layout is `${XDG_CACHE_HOME:-~/.cache}/scout/raw/YYYY-MM-DD/<HHMMSS>-<tool>-<id>.log`
// (§2.1).  Cache and not state, deliberately: `calls.jsonl` is the record,
// these blobs are disposable and bounded, and a user who wipes their cache has
// lost nothing but the ability to escalate on a summary from last week.
//
// Every function that touches the filesystem takes the base directory
// explicitly; the XDG resolution lives in a one-line wrapper on top.  Tests
// drive the `_in` forms against a tempdir, so nothing here needs an env var to
// be testable and none is invented.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Retention bounds for the spool (docs/wrap-watch.md §2.3).
///
/// Parsed from `[spool]` by `config::load_spool_config`; the defaults are what
/// a caller with no config file gets, and they only have to comfortably
/// outlive a working session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpoolConfig {
    pub max_age_days: u64,
    pub max_total_bytes: u64,
}

impl Default for SpoolConfig {
    fn default() -> Self {
        SpoolConfig { max_age_days: 7, max_total_bytes: 500 * 1024 * 1024 }
    }
}

/// What one sweep removed, for `scout gc` to report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Swept {
    pub files_deleted: u64,
    pub bytes_freed: u64,
    /// Total size of the blobs still in the spool afterwards.
    pub bytes_remaining: u64,
}

/// scout's cache directory: `$XDG_CACHE_HOME/scout`, falling back to
/// `$HOME/.cache/scout`, falling back to a relative `.cache/scout`.
///
/// Empty env values count as unset, matching `config::config_dir`'s rule and
/// the `${VAR:-...}` expansion the hooks use.
pub fn cache_dir() -> PathBuf {
    std::env::var("XDG_CACHE_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".cache"))
        })
        .unwrap_or_else(|| PathBuf::from(".cache"))
        .join("scout")
}

/// The spool root under `base`.
pub fn raw_dir(base: &Path) -> PathBuf {
    base.join("raw")
}

/// Persist one blob of captured output, then prune (docs/wrap-watch.md §2.3).
///
/// `call_id` is the id the caller's `stats::CallRecord` already minted — the
/// blob and the log row have to name each other, so no second id is invented
/// here.  Returns the path to record as `raw_path`, or `None` when the write
/// failed for any reason at all: a spool that cannot be written is a payload
/// without an escalation path, not a failed command.
pub fn write(tool: &str, call_id: &str, contents: &str, cfg: &SpoolConfig) -> Option<PathBuf> {
    write_in(&cache_dir(), tool, call_id, contents, cfg)
}

/// `write`, against an explicit base directory.
pub fn write_in(
    base: &Path,
    tool: &str,
    call_id: &str,
    contents: &str,
    cfg: &SpoolConfig,
) -> Option<PathBuf> {
    let now = SystemTime::now();
    let dir = raw_dir(base).join(day_dir(now));
    ensure_private_dir(base).ok()?;
    ensure_private_dir(&raw_dir(base)).ok()?;
    ensure_private_dir(&dir).ok()?;

    let path = dir.join(blob_name(now, tool, call_id));
    write_private(&path, contents).ok()?;

    // Prune on write, no daemon (§2.3): the cache tends itself as a side
    // effect of being used.  Cheap because the directory is small by design —
    // only filtered calls spool (§2.2).
    sweep_in(base, cfg);

    Some(path)
}

/// Create an empty blob for a job that will stream into it (docs/wait.md §3.1).
///
/// Unlike [`write`], this does not sweep and does not put contents on disk —
/// the caller appends as the child prints, and [`pin`]s the path so a
/// concurrent wrap's prune cannot delete a live log. Returns `None` when the
/// cache is unwritable; the job still runs, just without an escalation path.
pub fn create(tool: &str, call_id: &str) -> Option<PathBuf> {
    create_in(&cache_dir(), tool, call_id)
}

/// [`create`], against an explicit base directory.
pub fn create_in(base: &Path, tool: &str, call_id: &str) -> Option<PathBuf> {
    let now = SystemTime::now();
    let dir = raw_dir(base).join(day_dir(now));
    ensure_private_dir(base).ok()?;
    ensure_private_dir(&raw_dir(base)).ok()?;
    ensure_private_dir(&dir).ok()?;

    let path = dir.join(blob_name(now, tool, call_id));
    write_private(&path, "").ok()?;
    Some(path)
}

/// Append bytes to a blob opened by [`create`]. Fail-open: a full disk or a
/// vanished file returns `false` and the job keeps running.
pub fn append(path: &Path, bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    append_private(path, bytes).is_ok()
}

// ── Pin set ──────────────────────────────────────────────────────────────────
//
// Blobs backing a live job (or, later, a live watch) must survive GC. The
// registry that owns those jobs is in another module; the pin set lives here
// so every sweep — wrap's write path, `scout gc`, a job's own create — sees
// the same set without a circular dependency. docs/wrap-watch.md §2.3.

static PINS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

fn pins() -> &'static Mutex<HashSet<PathBuf>> {
    PINS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Keep `path` out of the next sweep. Idempotent. The matching [`unpin`]
/// belongs to whoever called this — typically the job leaving the registry.
pub fn pin(path: &Path) {
    if let Ok(mut set) = pins().lock() {
        set.insert(path.to_path_buf());
    }
}

/// Release a pin taken by [`pin`]. A path that was never pinned is a no-op.
pub fn unpin(path: &Path) {
    if let Ok(mut set) = pins().lock() {
        set.remove(path);
    }
}

fn is_pinned(path: &Path) -> bool {
    pins().lock().is_ok_and(|set| set.contains(path))
}

/// Delete blobs past `max_age_days`, then oldest-first until the spool is
/// under `max_total_bytes` (docs/wrap-watch.md §2.3).
pub fn sweep(cfg: &SpoolConfig) -> Swept {
    sweep_in(&cache_dir(), cfg)
}

/// `sweep`, against an explicit base directory.
///
/// Best-effort throughout: an unreadable entry is skipped rather than aborting
/// the sweep, and a missing spool is a sweep that found nothing to do.
pub fn sweep_in(base: &Path, cfg: &SpoolConfig) -> Swept {
    // docs/wrap-watch.md §2.3: blobs backing a live job (or watch) are
    // pinned. The set is process-wide so a wrap write's prune cannot
    // delete a detached job's log.
    let mut blobs = collect_blobs(&raw_dir(base));
    blobs.retain(|b| !is_pinned(&b.path));
    blobs.sort_by_key(|b| b.modified);

    let mut swept = Swept::default();
    let cutoff = SystemTime::now().checked_sub(Duration::from_secs(cfg.max_age_days * 86_400));
    let mut kept: Vec<Blob> = Vec::with_capacity(blobs.len());
    for blob in blobs {
        // A file the clock cannot place (mtime in the future, a checked_sub
        // that underflowed near the epoch) is kept by the age pass and left
        // to the size pass, which needs no clock to be right.
        if cutoff.is_some_and(|c| blob.modified < c) {
            remove(&blob, &mut swept);
        } else {
            kept.push(blob);
        }
    }

    let mut total: u64 = kept.iter().map(|b| b.bytes).sum();
    let mut oldest = kept.into_iter();
    while total > cfg.max_total_bytes {
        let Some(blob) = oldest.next() else { break };
        total -= blob.bytes;
        remove(&blob, &mut swept);
    }
    swept.bytes_remaining = total;

    prune_empty_days(&raw_dir(base));
    swept
}

/// Empty the spool outright — `scout gc --all`.
pub fn purge() -> Swept {
    purge_in(&cache_dir())
}

/// `purge`, against an explicit base directory.
///
/// Expressed as the ordinary sweep with both bounds at zero rather than as a
/// `remove_dir_all`, so "delete everything" walks the same code path as every
/// other deletion and reports the same counts.
pub fn purge_in(base: &Path) -> Swept {
    sweep_in(base, &SpoolConfig { max_age_days: 0, max_total_bytes: 0 })
}

// ── Naming ──────────────────────────────────────────────────────────────────

/// The `YYYY-MM-DD` directory for `t`.
///
/// UTC, not local time: std has no timezone database, and reaching for
/// `libc::localtime_r` would make a cache path depend on `$TZ` at write time.
/// The blob's own row in `calls.jsonl` carries the authoritative timestamp.
fn day_dir(t: SystemTime) -> String {
    let (y, m, d) = civil_from_days(unix_secs(t).div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// `<HHMMSS>-<tool>-<idfrag>.log` (docs/wrap-watch.md §2.1).
fn blob_name(t: SystemTime, tool: &str, call_id: &str) -> String {
    let secs_of_day = unix_secs(t).rem_euclid(86_400);
    let (h, m, s) = (secs_of_day / 3600, (secs_of_day % 3600) / 60, secs_of_day % 60);
    format!("{h:02}{m:02}{s:02}-{}-{}.log", head(tool, TOOL_CHARS, "tool"), id_frag(call_id))
}

const TOOL_CHARS: usize = 24;
const ID_FRAG_CHARS: usize = 12;

/// The id's tail, which is the unique half: `stats::next_id` mints
/// `<ms-hex>-<pid>-<seq>`, and the date directory plus `HHMMSS` already carry
/// what the leading millisecond stamp encodes.
fn id_frag(call_id: &str) -> String {
    let s = sanitize(call_id);
    // ASCII by construction after `sanitize`, so byte indexing is char indexing.
    let tail = if s.len() > ID_FRAG_CHARS { s[s.len() - ID_FRAG_CHARS..].to_string() } else { s };
    if tail.is_empty() {
        "anon".to_string()
    } else {
        tail
    }
}

fn head(s: &str, chars: usize, fallback: &str) -> String {
    let s = sanitize(s);
    let s = if s.len() > chars { s[..chars].to_string() } else { s };
    if s.is_empty() {
        fallback.to_string()
    } else {
        s
    }
}

/// Fold everything outside `[A-Za-z0-9]` to `-`.  Both halves of the name come
/// from a caller — a tool name and a call id — and a filename is not the place
/// to find out that one of them held a `/` or a newline.
fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

fn unix_secs(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_secs()).unwrap_or(i64::MAX),
    }
}

/// Days since the epoch → `(year, month, day)`, Howard Hinnant's civil-calendar
/// algorithm.  Fourteen lines of arithmetic against a whole date dependency,
/// for the one date string scout formats.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ── Filesystem ──────────────────────────────────────────────────────────────

struct Blob {
    path: PathBuf,
    bytes: u64,
    modified: SystemTime,
}

/// Every regular file in the spool, with the two facts the sweep needs.
///
/// Files directly under `raw/` are collected alongside the ones in date
/// directories: a stray blob from an older layout is still scout's to bound.
/// Nothing recurses further — the layout is exactly two levels deep.
fn collect_blobs(root: &Path) -> Vec<Blob> {
    fn blob_of(path: PathBuf) -> Option<Blob> {
        let meta = std::fs::metadata(&path).ok()?;
        if !meta.is_file() {
            return None;
        }
        Some(Blob { path, bytes: meta.len(), modified: meta.modified().ok()? })
    }

    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else { return out };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Ok(day) = std::fs::read_dir(&path) {
                out.extend(day.flatten().filter_map(|e| blob_of(e.path())));
            }
        } else if let Some(b) = blob_of(path) {
            out.push(b);
        }
    }
    out
}

/// Delete one blob, counting it only if the unlink actually happened — a
/// deletion that failed has not freed anything, and `scout gc` must not claim
/// it did.
fn remove(blob: &Blob, swept: &mut Swept) {
    if std::fs::remove_file(&blob.path).is_ok() {
        swept.files_deleted += 1;
        swept.bytes_freed += blob.bytes;
    }
}

/// Drop date directories the sweep emptied.  `remove_dir` refuses a non-empty
/// directory, which is the whole guard: no check-then-delete race to lose.
fn prune_empty_days(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let _ = std::fs::remove_dir(&path);
        }
    }
}

/// Create `dir` with mode `0700` if it does not already exist, leaving an
/// existing directory's mode alone.
///
/// Same rule and same reasoning as `config::ensure_private_dir` and
/// `stats::ensure_private_dir`: raw command output can contain anything
/// (docs/wrap-watch.md §2.1), so a directory scout creates must not land at
/// the process umask — and a mode the user set on purpose is not ours to
/// override after the fact.
#[cfg(unix)]
fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    use std::os::unix::fs::DirBuilderExt;
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(not(unix))]
fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// Write `contents` to `path`, creating it `0600`.
///
/// `mode` applies at creation only, so the rare same-id rewrite reuses
/// whatever mode the existing file has rather than resetting it.
#[cfg(unix)]
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(contents.as_bytes())
}

#[cfg(not(unix))]
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    std::fs::write(path, contents)
}

/// Append `bytes` to an existing blob, creating nothing.
#[cfg(unix)]
fn append_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().append(true).mode(0o600).open(path)?;
    f.write_all(bytes)
}

#[cfg(not(unix))]
fn append_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().append(true).open(path)?;
    f.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn cfg(days: u64, bytes: u64) -> SpoolConfig {
        SpoolConfig { max_age_days: days, max_total_bytes: bytes }
    }

    /// Backdate a blob so the age pass has something to find.  The sweep reads
    /// mtime, so mtime is what a test has to move.
    fn backdate(path: &Path, days: u64) {
        let when = SystemTime::now() - Duration::from_secs(days * 86_400);
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(when)).unwrap();
    }

    fn spooled(base: &Path) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> =
            collect_blobs(&raw_dir(base)).into_iter().map(|b| b.path).collect();
        paths.sort();
        paths
    }

    #[test]
    fn write_puts_the_blob_under_a_dated_directory_named_by_time_tool_and_call_id() {
        let dir = TempDir::new().unwrap();
        let path =
            write_in(dir.path(), "wrap", "18f2c3-4471-7", "hello\n", &SpoolConfig::default())
                .expect("a writable base must spool");

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n");
        assert_eq!(path.parent().unwrap().parent().unwrap(), raw_dir(dir.path()));

        let day = path.parent().unwrap().file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(day, day_dir(SystemTime::now()), "the date directory is today's");
        assert_eq!(day.len(), 10, "YYYY-MM-DD: {day}");

        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.ends_with("-wrap-8f2c3-4471-7.log"), "{name}");
        assert_eq!(name.split('-').next().unwrap().len(), 6, "HHMMSS prefix: {name}");
    }

    #[cfg(unix)]
    #[test]
    fn the_blob_is_0600_and_every_directory_scout_creates_is_0700() {
        use std::os::unix::fs::PermissionsExt;
        let mode_of = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;

        let tmp = TempDir::new().unwrap();
        // Nested, so the base itself is scout's to create too — raw command
        // output can contain anything (docs/wrap-watch.md §2.1).
        let base = tmp.path().join("cache").join("scout");
        let path = write_in(&base, "wrap", "a3f9", "secret\n", &SpoolConfig::default()).unwrap();

        assert_eq!(mode_of(&path), 0o600);
        assert_eq!(mode_of(path.parent().unwrap()), 0o700, "the date directory");
        assert_eq!(mode_of(&raw_dir(&base)), 0o700, "raw/");
        assert_eq!(mode_of(&base), 0o700, "the spool base");
    }

    #[test]
    fn a_tool_name_or_call_id_carrying_path_separators_cannot_escape_the_day_directory() {
        let dir = TempDir::new().unwrap();
        let path =
            write_in(dir.path(), "../../wrap", "../../../etc/passwd", "x", &SpoolConfig::default())
                .unwrap();
        assert_eq!(path.parent().unwrap().parent().unwrap(), raw_dir(dir.path()));
        assert_eq!(spooled(dir.path()), vec![path]);
    }

    #[test]
    fn sweep_deletes_blobs_older_than_max_age_days_and_keeps_the_rest() {
        let dir = TempDir::new().unwrap();
        let old = write_in(dir.path(), "wrap", "old", "o", &cfg(7, u64::MAX)).unwrap();
        let fresh = write_in(dir.path(), "wrap", "fresh", "f", &cfg(7, u64::MAX)).unwrap();
        backdate(&old, 30);

        let swept = sweep_in(dir.path(), &cfg(7, u64::MAX));
        assert_eq!(swept.files_deleted, 1);
        assert_eq!(swept.bytes_freed, 1);
        assert_eq!(spooled(dir.path()), vec![fresh]);
        assert!(!old.exists());
    }

    #[test]
    fn sweep_drops_a_date_directory_it_emptied() {
        let dir = TempDir::new().unwrap();
        let old = write_in(dir.path(), "wrap", "old", "o", &cfg(7, u64::MAX)).unwrap();
        backdate(&old, 30);
        sweep_in(dir.path(), &cfg(7, u64::MAX));
        assert!(!old.parent().unwrap().exists(), "an emptied day leaves no directory behind");
        assert!(raw_dir(dir.path()).exists(), "...but the spool root stays");
    }

    #[test]
    fn sweep_deletes_oldest_first_until_the_total_is_under_max_total_bytes() {
        let dir = TempDir::new().unwrap();
        // Three 100-byte blobs, aged so their order is unambiguous.
        let blobs: Vec<PathBuf> = ["oldest", "middle", "newest"]
            .iter()
            .map(|id| {
                write_in(dir.path(), "wrap", id, &"x".repeat(100), &cfg(30, u64::MAX)).unwrap()
            })
            .collect();
        backdate(&blobs[0], 3);
        backdate(&blobs[1], 2);
        backdate(&blobs[2], 1);

        let swept = sweep_in(dir.path(), &cfg(30, 250));
        assert_eq!(swept.files_deleted, 1, "one blob is enough to get under 250");
        assert_eq!(swept.bytes_freed, 100);
        assert_eq!(swept.bytes_remaining, 200);
        assert!(!blobs[0].exists(), "the oldest goes first");
        assert!(blobs[1].exists() && blobs[2].exists());
    }

    #[test]
    fn sweep_deletes_nothing_when_the_spool_is_under_both_bounds() {
        let dir = TempDir::new().unwrap();
        let a = write_in(dir.path(), "wrap", "a", "aaa", &SpoolConfig::default()).unwrap();
        let b = write_in(dir.path(), "wrap", "b", "bbb", &SpoolConfig::default()).unwrap();

        let swept = sweep_in(dir.path(), &SpoolConfig::default());
        assert_eq!(swept, Swept { files_deleted: 0, bytes_freed: 0, bytes_remaining: 6 });
        let mut both = vec![a, b];
        both.sort();
        assert_eq!(spooled(dir.path()), both);
    }

    #[test]
    fn sweeping_a_spool_that_was_never_written_is_not_an_error() {
        let dir = TempDir::new().unwrap();
        assert_eq!(sweep_in(dir.path(), &SpoolConfig::default()), Swept::default());
    }

    #[test]
    fn every_write_sweeps_so_the_spool_stays_bounded_without_a_daemon() {
        let dir = TempDir::new().unwrap();
        let bounds = cfg(30, 150);
        let first = write_in(dir.path(), "wrap", "first", &"x".repeat(100), &bounds).unwrap();
        backdate(&first, 1);
        let second = write_in(dir.path(), "wrap", "second", &"y".repeat(100), &bounds).unwrap();

        assert!(!first.exists(), "the second write evicted the first");
        assert_eq!(spooled(dir.path()), vec![second]);
    }

    #[test]
    fn purge_empties_the_spool_and_reports_what_it_freed() {
        let dir = TempDir::new().unwrap();
        for id in ["a", "b", "c"] {
            write_in(dir.path(), "wrap", id, &"x".repeat(10), &SpoolConfig::default()).unwrap();
        }
        let swept = purge_in(dir.path());
        assert_eq!(swept, Swept { files_deleted: 3, bytes_freed: 30, bytes_remaining: 0 });
        assert!(spooled(dir.path()).is_empty());
    }

    #[test]
    fn a_base_directory_that_cannot_be_created_degrades_to_no_spool() {
        // /dev/null is not a directory and never will be: the same fail-open
        // probe `stats` uses, and the whole contract of this module — a broken
        // spool costs the caller its escalation path, never its result.
        let base = PathBuf::from("/dev/null/scout");
        assert!(write_in(&base, "wrap", "id", "out", &SpoolConfig::default()).is_none());
        assert_eq!(sweep_in(&base, &SpoolConfig::default()), Swept::default());
    }

    #[cfg(unix)]
    #[test]
    fn an_unwritable_spool_directory_degrades_to_no_spool() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let day = raw_dir(dir.path()).join(day_dir(SystemTime::now()));
        std::fs::create_dir_all(&day).unwrap();
        std::fs::set_permissions(&day, std::fs::Permissions::from_mode(0o500)).unwrap();

        let result = write_in(dir.path(), "wrap", "id", "out", &SpoolConfig::default());

        // Restore before the assert so the TempDir can always clean itself up.
        std::fs::set_permissions(&day, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_none(), "an unwritable day directory is not the caller's problem");
    }

    #[test]
    fn the_day_directory_and_blob_name_agree_with_the_documented_shape() {
        // docs/wrap-watch.md §2.1: raw/2026-08-15/143212-wrap-a3f9.log
        let t = UNIX_EPOCH + Duration::from_secs(1_786_804_332);
        assert_eq!(day_dir(t), "2026-08-15");
        assert_eq!(blob_name(t, "wrap", "a3f9"), "143212-wrap-a3f9.log");
    }

    #[test]
    fn civil_from_days_handles_the_epoch_leap_years_and_century_rules() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(59), (1970, 3, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29), "2000 is a leap year");
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn an_empty_tool_or_call_id_still_produces_a_usable_name() {
        let name = blob_name(UNIX_EPOCH, "", "");
        assert_eq!(name, "000000-tool-anon.log");
    }

    #[test]
    fn cache_dir_honours_xdg_cache_home_and_falls_back_to_home() {
        // Env is process-global; these two reads are the only ones in this
        // module, so a local lock is enough.
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let saved = (std::env::var("XDG_CACHE_HOME").ok(), std::env::var("HOME").ok());

        std::env::set_var("XDG_CACHE_HOME", "/xdg/cache");
        assert_eq!(cache_dir(), PathBuf::from("/xdg/cache/scout"));

        // Empty counts as unset, as everywhere else in scout.
        std::env::set_var("XDG_CACHE_HOME", "");
        std::env::set_var("HOME", "/home/tester");
        assert_eq!(cache_dir(), PathBuf::from("/home/tester/.cache/scout"));

        for (key, value) in [("XDG_CACHE_HOME", saved.0), ("HOME", saved.1)] {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }

    #[test]
    fn create_makes_an_empty_blob_and_append_extends_it() {
        let dir = TempDir::new().unwrap();
        let path = create_in(dir.path(), "wrap", "live-1").expect("writable base");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        assert!(append(&path, b"hello\n"));
        assert!(append(&path, b"world\n"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\nworld\n");
    }

    #[test]
    fn a_pinned_blob_survives_a_purge() {
        let dir = TempDir::new().unwrap();
        let live = create_in(dir.path(), "wrap", "live").unwrap();
        let stale = write_in(dir.path(), "wrap", "stale", "gone", &SpoolConfig::default()).unwrap();
        pin(&live);

        let swept = purge_in(dir.path());
        assert!(live.exists(), "a live job's log must survive GC");
        assert!(!stale.exists(), "an unpinned blob is still reclaimable");
        assert_eq!(swept.files_deleted, 1, "only the unpinned blob was removed");

        unpin(&live);
        let swept = purge_in(dir.path());
        assert!(!live.exists(), "unpinning returns the blob to ordinary GC");
        assert_eq!(swept.files_deleted, 1);
    }
}
