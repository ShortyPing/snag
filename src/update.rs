//! `snag update` — replace the running binary with the latest release, and the
//! background check behind the "an update is available" notice.
//!
//! The notice must never cost a run anything: startup reads a cache file and
//! nothing else, and the refresh that fills that cache happens on a detached
//! thread. The whole feature is off unless stderr is a terminal, so a CI job
//! makes no network request and prints no notice.

use crate::revision::{self, Revision, TARGET, VERSION};
use crate::{Command, Ctx, Exit, Format, UpdateArgs};
use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long a check result stays good. A release is not worth a request per run.
const CHECK_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Percentage of eligible runs that actually print the notice. An out-of-date
/// binary is worth mentioning, not worth saying on every single invocation.
const NOTICE_CHANCE: u32 = 30;

/// Metadata is small; a slow network should not keep a detached thread alive.
const METADATA_TIMEOUT: Duration = Duration::from_secs(10);

/// The binary is several megabytes, and `snag update` is an explicit request.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// Set to any non-empty value to disable both the check and the notice.
const OPT_OUT: &str = "SNAG_NO_UPDATE_CHECK";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Cached {
    /// Unix seconds. Used only to age the entry out.
    checked_at: u64,
    version: String,
    tag: String,
}

// ---------------------------------------------------------------------------
// version comparison
// ---------------------------------------------------------------------------

/// `major.minor.patch`, ignoring any `-prerelease` or `+build` suffix. Returns
/// `None` for anything that does not parse, so an unrecognised version is
/// treated as "cannot compare" rather than "newer".
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Whether `candidate` is strictly newer than `current`. A version that will
/// not parse never counts as newer — a local build with an odd version string
/// should not be nagged at.
fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(new), Some(now)) => new > now,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// the startup notice
// ---------------------------------------------------------------------------

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `percent` of seeds pass. Split out from the clock so the distribution is
/// testable without depending on what time it is.
fn roll_with(seed: u64, percent: u32) -> bool {
    // subsec_nanos on its own clusters badly on coarse clocks; scramble first.
    let x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let x = x ^ (x >> 31);
    (x % 100) < u64::from(percent)
}

fn roll(percent: u32) -> bool {
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()));
    roll_with(seed, percent)
}

fn cache_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
    };
    Some(base?.join("snag").join("update-check.json"))
}

fn read_cache() -> Option<Cached> {
    let body = std::fs::read_to_string(cache_path()?).ok()?;
    serde_json::from_str(&body).ok()
}

/// Written through a temp file so a process killed mid-write cannot leave a
/// half-parsed cache behind.
fn write_cache(entry: &Cached) -> anyhow::Result<()> {
    let path = cache_path().context("no cache directory available")?;
    let dir = path.parent().context("cache path has no parent")?;
    std::fs::create_dir_all(dir)?;

    let tmp = dir.join(format!("update-check.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(entry)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn is_stale(entry: Option<&Cached>) -> bool {
    match entry {
        None => true,
        Some(c) => now_secs().saturating_sub(c.checked_at) >= CHECK_TTL.as_secs(),
    }
}

/// Commands that must not be disturbed: `update` reports for itself, and the
/// other two are consumed by machines even when a terminal is attached.
fn command_wants_notice(command: &Command) -> bool {
    !matches!(
        command,
        Command::Update(_) | Command::Completions { .. } | Command::Revision(_)
    )
}

fn enabled(ctx: &Ctx, command: &Command) -> bool {
    std::env::var_os(OPT_OUT).is_none_or(|v| v.is_empty())
        && ctx.format == Format::Human
        && !ctx.quiet
        && std::io::stderr().is_terminal()
        && command_wants_notice(command)
}

/// Print the notice if the cache already knows about a newer release, then
/// refresh the cache in the background when it has aged out.
///
/// Deliberately does no network work on the calling thread: a check that made
/// every invocation wait on github.com would be worse than no check at all.
pub fn start_check(ctx: &Ctx, command: &Command) {
    if !enabled(ctx, command) {
        return;
    }

    let cached = read_cache();

    if let Some(entry) = &cached
        && is_newer(&entry.version, VERSION)
        && roll(NOTICE_CHANCE)
    {
        eprintln!(
            "snag {} is available (you have {VERSION}) — run `snag update`",
            entry.version
        );
    }

    if is_stale(cached.as_ref()) {
        let repo = revision::DEFAULT_REPO;
        // Detached: process exit may kill it mid-request, and that is fine —
        // the cache stays stale and the next run tries again.
        std::thread::spawn(move || {
            if let Ok(latest) = fetch(&revision::latest_url(repo), METADATA_TIMEOUT) {
                let _ = write_cache(&Cached {
                    checked_at: now_secs(),
                    version: latest.version,
                    tag: latest.tag,
                });
            }
        });
    }
}

// ---------------------------------------------------------------------------
// the update itself
// ---------------------------------------------------------------------------

fn client(timeout: Duration) -> anyhow::Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()?)
}

fn fetch(url: &str, timeout: Duration) -> anyhow::Result<Revision> {
    let body = client(timeout)?
        .get(url)
        .send()?
        .error_for_status()?
        .text()?;
    serde_json::from_str(&body).with_context(|| format!("parsing {url}"))
}

/// Give the replacement the same permissions a release binary needs. No-op on
/// Windows, where executability is decided by the extension.
#[cfg(unix)]
fn make_executable(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("marking {} executable", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// Run the downloaded binary and confirm it reports the version we asked for.
/// This is the only check that the bytes are the right program and arrived
/// whole — the manifest carries no checksum, so a truncated or mis-tagged
/// asset would otherwise be installed silently.
fn verify(path: &Path, expected: &str) -> anyhow::Result<()> {
    let out = std::process::Command::new(path)
        .arg("--version")
        .output()
        .with_context(|| format!("running {} --version", path.display()))?;

    if !out.status.success() {
        bail!(
            "the downloaded binary exited {} instead of printing its version",
            out.status
        );
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    let reported = stdout.split_whitespace().nth(1).unwrap_or("");
    if reported != expected {
        bail!("expected the download to report version {expected}, got {reported:?}");
    }
    Ok(())
}

/// Swap `replacement` in for `current`.
///
/// A running executable cannot be overwritten in place, but its directory entry
/// can be repointed: on Unix the rename is atomic and the old inode stays alive
/// for this process. Windows refuses to rename onto a running image, so the old
/// one is moved aside first and cleaned up on a later run.
fn install(current: &Path, replacement: &Path) -> anyhow::Result<()> {
    if cfg!(windows) {
        let backup = current.with_extension("old");
        let _ = std::fs::remove_file(&backup);
        std::fs::rename(current, &backup)
            .with_context(|| format!("moving {} aside", current.display()))?;

        if let Err(err) = std::fs::rename(replacement, current) {
            // Put the working binary back rather than leaving nothing installed.
            let _ = std::fs::rename(&backup, current);
            return Err(err).with_context(|| format!("installing {}", current.display()));
        }
        // Still mapped by this process; the next update removes it.
        let _ = std::fs::remove_file(&backup);
        return Ok(());
    }

    std::fs::rename(replacement, current)
        .with_context(|| format!("installing {}", current.display()))
}

fn denied(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied)
    })
}

fn download(url: &str, into: &Path) -> anyhow::Result<()> {
    let bytes = client(DOWNLOAD_TIMEOUT)?
        .get(url)
        .send()
        .with_context(|| format!("downloading {url}"))?
        .error_for_status()
        .with_context(|| format!("downloading {url}"))?
        .bytes()?;

    if bytes.is_empty() {
        bail!("{url} returned an empty file");
    }

    std::fs::write(into, &bytes).with_context(|| format!("writing {}", into.display()))?;
    Ok(())
}

pub fn cmd_update(ctx: &Ctx, args: UpdateArgs) -> anyhow::Result<Exit> {
    let source = match (&args.manifest, &args.tag) {
        (Some(manifest), _) => manifest.clone(),
        (None, Some(tag)) => revision::tag_url(&args.repo, tag),
        (None, None) => revision::latest_url(&args.repo),
    };

    let latest = revision::load(&source)
        .with_context(|| format!("looking up the release manifest at {source}"))?;

    let up_to_date = !is_newer(&latest.version, VERSION);

    if args.check {
        if up_to_date {
            println!("snag {VERSION} is up to date");
        } else {
            println!("snag {} is available (you have {VERSION})", latest.version);
        }
        // Record it either way, so an explicit check also quiets the notice.
        let _ = write_cache(&Cached {
            checked_at: now_secs(),
            version: latest.version,
            tag: latest.tag,
        });
        return Ok(Exit::Ok);
    }

    if up_to_date && !args.force {
        if !ctx.quiet {
            println!("snag {VERSION} is already up to date");
        }
        return Ok(Exit::Ok);
    }

    let platform = latest.platforms.get(TARGET).with_context(|| {
        let known: Vec<&str> = latest.platforms.keys().map(String::as_str).collect();
        format!(
            "release {} has no binary for {TARGET} (it has: {})",
            latest.tag,
            known.join(", ")
        )
    })?;

    let current = std::env::current_exe().context("locating the running binary")?;
    let dir = current
        .parent()
        .context("the running binary has no parent directory")?;

    // Staged beside the current binary so the install is a rename within one
    // filesystem; a temp dir could be on another and rename would fail.
    let staged = dir.join(format!(".snag-update-{}", std::process::id()));

    let result = (|| -> anyhow::Result<()> {
        download(&platform.url, &staged)?;
        make_executable(&staged)?;
        verify(&staged, &latest.version)?;
        install(&current, &staged)
    })();

    if let Err(err) = result {
        let _ = std::fs::remove_file(&staged);
        // Only the permission case earns the sudo hint. Appending it to every
        // failure would bury the reason the update actually stopped.
        if denied(&err) {
            return Err(err).with_context(|| {
                format!(
                    "updating {} needs write access to {}",
                    current.display(),
                    dir.display()
                )
            });
        }
        return Err(err);
    }

    let _ = write_cache(&Cached {
        checked_at: now_secs(),
        version: latest.version.clone(),
        tag: latest.tag.clone(),
    });

    if !ctx.quiet {
        if latest.version == VERSION {
            println!(
                "reinstalled {} at {VERSION} ({})",
                current.display(),
                latest.tag
            );
        } else {
            println!(
                "updated {} from {VERSION} to {} ({})",
                current.display(),
                latest.version,
                latest.tag
            );
        }
    }
    Ok(Exit::Ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_plain_version() {
        assert_eq!(parse_version("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
    }

    #[test]
    fn ignores_prerelease_and_build_suffixes() {
        assert_eq!(parse_version("1.2.3-rc1"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2.3+deadbeef"), Some((1, 2, 3)));
    }

    #[test]
    fn refuses_versions_it_cannot_compare() {
        for bad in ["", "1.2", "1.2.3.4", "next", "1.x.3"] {
            assert_eq!(parse_version(bad), None, "{bad} should not parse");
        }
    }

    #[test]
    fn compares_each_component_numerically() {
        assert!(is_newer("0.10.0", "0.9.0"), "10 > 9, not \"10\" < \"9\"");
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(is_newer("0.1.1", "0.1.0"));
    }

    #[test]
    fn an_equal_or_older_version_is_not_newer() {
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
    }

    // A local build with an odd version should never be nagged at, so anything
    // unparseable has to fall on the "not newer" side.
    #[test]
    fn an_unparseable_version_is_never_newer() {
        assert!(!is_newer("nightly", "0.1.0"));
        assert!(!is_newer("0.2.0", "dev"));
    }

    #[test]
    fn the_notice_roll_honours_its_bounds() {
        for seed in 0..1_000 {
            assert!(roll_with(seed, 100), "100% must always fire, seed {seed}");
            assert!(!roll_with(seed, 0), "0% must never fire, seed {seed}");
        }
    }

    #[test]
    fn the_notice_roll_is_roughly_uniform() {
        let hits = (0..10_000).filter(|s| roll_with(*s, NOTICE_CHANCE)).count();
        // Wide bounds: this asserts the scramble is not degenerate, not that it
        // is a good PRNG.
        assert!(
            (2_500..3_500).contains(&hits),
            "expected about 3000 of 10000 at {NOTICE_CHANCE}%, got {hits}"
        );
    }

    #[test]
    fn a_missing_cache_entry_is_stale() {
        assert!(is_stale(None));
    }

    #[test]
    fn a_fresh_entry_is_not_stale_and_an_aged_one_is() {
        let fresh = Cached {
            checked_at: now_secs(),
            version: "0.9.0".into(),
            tag: "v0.9.0".into(),
        };
        assert!(!is_stale(Some(&fresh)));

        let old = Cached {
            checked_at: now_secs() - CHECK_TTL.as_secs() - 1,
            ..fresh
        };
        assert!(is_stale(Some(&old)));
    }

    // A clock that moved backwards must not make an entry look infinitely
    // fresh, nor panic on the subtraction.
    #[test]
    fn a_future_timestamp_does_not_panic() {
        let future = Cached {
            checked_at: now_secs() + 10_000,
            version: "0.9.0".into(),
            tag: "v0.9.0".into(),
        };
        assert!(!is_stale(Some(&future)));
    }

    #[test]
    fn machine_formats_and_quiet_runs_get_no_notice() {
        let human = Ctx {
            format: Format::Human,
            color: false,
            verbose: 0,
            quiet: false,
        };
        // stderr is not a terminal under `cargo test`, which alone disables it.
        assert!(!enabled(&human, &crate::default_run()));

        let quiet = Ctx {
            quiet: true,
            ..human
        };
        assert!(!enabled(&quiet, &crate::default_run()));
    }

    #[test]
    fn update_and_completions_never_show_the_notice() {
        assert!(!command_wants_notice(&Command::Update(UpdateArgs {
            check: false,
            force: false,
            tag: None,
            manifest: None,
            repo: revision::DEFAULT_REPO.to_string(),
        })));
        assert!(!command_wants_notice(&Command::Completions {
            shell: clap_complete::Shell::Bash,
        }));
        assert!(command_wants_notice(&crate::default_run()));
    }

    #[test]
    fn only_permission_errors_earn_the_write_access_hint() {
        let denied_err = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "nope",
        ))
        .context("installing /usr/local/bin/snag");
        assert!(denied(&denied_err));

        let other = anyhow::anyhow!("expected the download to report version 9.9.9, got \"0.1.0\"");
        assert!(!denied(&other));
    }

    #[test]
    fn the_cache_round_trips() {
        let entry = Cached {
            checked_at: 1_700_000_000,
            version: "0.2.0".into(),
            tag: "v0.2.0".into(),
        };
        let json = serde_json::to_string(&entry).expect("serialize");
        let back: Cached = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(entry, back);
    }
}
