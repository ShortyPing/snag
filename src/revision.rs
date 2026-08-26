//! `snag revision` — emit the release manifest (`revision.json`) describing this
//! build: its version, the commit it was built from, and a download URL per
//! platform. The release workflow runs it on every tag so consumers can resolve
//! "latest" without scraping the releases page.

use crate::{Ctx, Exit, RevisionArgs};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime};

/// Default repository the download URLs point at.
pub const DEFAULT_REPO: &str = "ShortyPing/snag";

/// The version this binary was compiled as.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The commit this binary was compiled from, baked in by `build.rs`.
pub const COMMIT: &str = env!("SNAG_GIT_COMMIT");

/// The target triple this binary was compiled for, baked in by `build.rs`.
/// Matches a key in `platforms`.
pub const TARGET: &str = env!("SNAG_TARGET");

/// The build number a repository with no previous release starts at.
pub const FIRST_BUILD: u64 = 1;

/// Cap on the previous-revision lookup, so a generate step cannot hang a
/// release job waiting on a network that is not answering.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);

/// The release matrix, kept in step with the `build` job in the workflows.
/// `(target triple, os, arch, executable suffix)`.
const TARGETS: &[(&str, &str, &str, &str)] = &[
    ("x86_64-unknown-linux-gnu", "linux", "x86_64", ""),
    ("aarch64-apple-darwin", "macos", "aarch64", ""),
    ("x86_64-pc-windows-msvc", "windows", "x86_64", ".exe"),
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revision {
    pub version: String,
    /// Monotonic across releases: one past the previous release's number.
    /// Defaulted so a file written before this field existed still parses.
    #[serde(default = "first_build")]
    pub build: u64,
    pub commit: String,
    pub tag: String,
    pub repository: String,
    pub released_at: String,
    /// Keyed by target triple; `BTreeMap` so the file is byte-stable.
    pub platforms: BTreeMap<String, Platform>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Platform {
    pub os: String,
    pub arch: String,
    pub target: String,
    pub binary: String,
    pub url: String,
}

fn first_build() -> u64 {
    FIRST_BUILD
}

impl Revision {
    pub fn new(
        version: &str,
        build: u64,
        commit: &str,
        tag: &str,
        repo: &str,
        released_at: &str,
    ) -> Self {
        let platforms = TARGETS
            .iter()
            .map(|(target, os, arch, suffix)| {
                let binary = format!("snag-{target}{suffix}");
                let url = format!("https://github.com/{repo}/releases/download/{tag}/{binary}");
                (
                    (*target).to_string(),
                    Platform {
                        os: (*os).to_string(),
                        arch: (*arch).to_string(),
                        target: (*target).to_string(),
                        binary,
                        url,
                    },
                )
            })
            .collect();

        Revision {
            version: version.to_string(),
            build,
            commit: commit.to_string(),
            tag: tag.to_string(),
            repository: repo.to_string(),
            released_at: released_at.to_string(),
            platforms,
        }
    }
}

/// The newest release's manifest, which GitHub resolves without knowing its
/// tag — the assets carry no version in their names.
pub fn latest_url(repo: &str) -> String {
    format!("https://github.com/{repo}/releases/latest/download/revision.json")
}

/// One specific release's manifest.
pub fn tag_url(repo: &str, tag: &str) -> String {
    format!("https://github.com/{repo}/releases/download/{tag}/revision.json")
}

/// Read a manifest from a file path or an `http(s)` URL.
pub fn load(source: &str) -> anyhow::Result<Revision> {
    let body = if source.starts_with("https://") || source.starts_with("http://") {
        reqwest::blocking::Client::builder()
            .timeout(LOOKUP_TIMEOUT)
            .build()?
            .get(source)
            .send()?
            .error_for_status()?
            .text()?
    } else {
        std::fs::read_to_string(source).with_context(|| format!("reading {source}"))?
    };

    serde_json::from_str(&body).with_context(|| format!("parsing {source}"))
}

fn next_build(previous: &Revision) -> u64 {
    previous.build.saturating_add(1)
}

/// One past the previous release's build number, or `FIRST_BUILD` when there is
/// nothing to increment from. A failed lookup is not fatal — the first release
/// of a repository has no predecessor — but it is always reported, because the
/// same fallback fires on a network blip and would silently rewind the counter.
fn resolve_build(args: &RevisionArgs) -> u64 {
    if let Some(n) = args.build {
        return n;
    }
    if args.offline {
        return FIRST_BUILD;
    }

    let source = args
        .previous
        .clone()
        .unwrap_or_else(|| latest_url(&args.repo));

    match load(&source) {
        Ok(previous) => next_build(&previous),
        Err(err) => {
            eprintln!("warning: no previous revision at {source} ({err:#})");
            eprintln!("warning: starting the build number at {FIRST_BUILD}");
            FIRST_BUILD
        }
    }
}

pub fn cmd_revision(ctx: &Ctx, args: RevisionArgs) -> anyhow::Result<Exit> {
    let tag = args.tag.clone().unwrap_or_else(|| format!("v{VERSION}"));
    let commit = args.commit.clone().unwrap_or_else(|| COMMIT.to_string());
    let released_at = humantime::format_rfc3339_seconds(SystemTime::now()).to_string();
    let build = resolve_build(&args);

    let revision = Revision::new(VERSION, build, &commit, &tag, &args.repo, &released_at);
    let mut json = serde_json::to_string_pretty(&revision)?;
    json.push('\n');

    if args.output == Path::new("-") {
        let mut out = std::io::stdout().lock();
        out.write_all(json.as_bytes())?;
        out.flush()?;
        return Ok(Exit::Ok);
    }

    if let Some(dir) = args.output.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&args.output, &json)
        .with_context(|| format!("writing {}", args.output.display()))?;

    if !ctx.quiet {
        println!(
            "wrote {} ({} build {} {} at {})",
            args.output.display(),
            revision.tag,
            revision.build,
            revision.commit,
            revision.released_at
        );
    }

    Ok(Exit::Ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Revision {
        Revision::new(
            "0.2.0",
            7,
            "41795ba0000000000000000000000000000000aa",
            "v0.2.0",
            "ShortyPing/snag",
            "2026-08-26T12:00:00Z",
        )
    }

    fn write(name: &str, body: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("snag-revision-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn lists_every_release_target() {
        let rev = sample();
        assert_eq!(
            rev.platforms.len(),
            TARGETS.len(),
            "expected {} platforms, got {}",
            TARGETS.len(),
            rev.platforms.len()
        );
        for (target, ..) in TARGETS {
            assert!(
                rev.platforms.contains_key(*target),
                "missing platform {target}"
            );
        }
    }

    #[test]
    fn download_url_matches_the_uploaded_asset_name() {
        let rev = sample();
        let linux = &rev.platforms["x86_64-unknown-linux-gnu"];
        assert_eq!(linux.binary, "snag-x86_64-unknown-linux-gnu");
        assert_eq!(
            linux.url,
            "https://github.com/ShortyPing/snag/releases/download/v0.2.0/snag-x86_64-unknown-linux-gnu"
        );
    }

    #[test]
    fn windows_asset_keeps_its_exe_suffix() {
        let rev = sample();
        let win = &rev.platforms["x86_64-pc-windows-msvc"];
        assert_eq!(win.binary, "snag-x86_64-pc-windows-msvc.exe");
        assert!(
            win.url.ends_with("/snag-x86_64-pc-windows-msvc.exe"),
            "expected .exe suffix in url, got {}",
            win.url
        );
    }

    #[test]
    fn url_follows_the_tag_not_the_version() {
        let rev = Revision::new(
            "0.2.0",
            1,
            "abc",
            "nightly",
            "acme/snag",
            "2026-08-26T12:00:00Z",
        );
        assert_eq!(rev.version, "0.2.0");
        assert!(
            rev.platforms["aarch64-apple-darwin"]
                .url
                .starts_with("https://github.com/acme/snag/releases/download/nightly/"),
            "got {}",
            rev.platforms["aarch64-apple-darwin"].url
        );
    }

    #[test]
    fn round_trips_through_json() {
        let rev = sample();
        let json = serde_json::to_string(&rev).expect("serialize");
        let back: Revision = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(rev, back);
    }

    #[test]
    fn platform_order_is_stable() {
        let json = serde_json::to_string(&sample()).expect("serialize");
        let first = json
            .find("aarch64-apple-darwin")
            .expect("darwin key present");
        let second = json
            .find("x86_64-pc-windows-msvc")
            .expect("windows key present");
        assert!(first < second, "keys should be sorted, got {json}");
    }

    #[test]
    fn build_stamps_a_commit() {
        assert!(!COMMIT.is_empty(), "build.rs must set SNAG_GIT_COMMIT");
    }

    #[test]
    fn build_number_increments_from_the_previous() {
        assert_eq!(next_build(&sample()), 8);
    }

    #[test]
    fn build_number_saturates_instead_of_wrapping() {
        let mut rev = sample();
        rev.build = u64::MAX;
        assert_eq!(next_build(&rev), u64::MAX);
    }

    // A revision.json written before the field existed must still parse, and it
    // must not read as 0 — the next release would then repeat build 1.
    #[test]
    fn a_revision_without_a_build_field_reads_as_the_first_build() {
        let json = r#"{
            "version": "0.1.0",
            "commit": "abc",
            "tag": "v0.1.0",
            "repository": "ShortyPing/snag",
            "released_at": "2026-08-26T12:00:00Z",
            "platforms": {}
        }"#;
        let rev: Revision = serde_json::from_str(json).expect("parse");
        assert_eq!(rev.build, FIRST_BUILD);
        assert_eq!(next_build(&rev), 2);
    }

    #[test]
    fn load_reads_a_local_file() {
        let mut json = serde_json::to_string(&sample()).unwrap();
        json.push('\n');
        let path = write("previous-ok.json", &json);

        let loaded = load(path.to_str().unwrap()).expect("load");
        assert_eq!(loaded.build, 7, "got {}", loaded.build);
        assert_eq!(next_build(&loaded), 8);
    }

    #[test]
    fn load_says_which_file_it_could_not_parse() {
        let path = write("previous-garbage.json", "not json at all");
        let err = load(path.to_str().unwrap()).expect_err("should fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("parsing") && msg.contains("previous-garbage.json"),
            "expected the path and \"parsing\" in the error, got {msg}"
        );
    }

    #[test]
    fn load_says_which_file_is_missing() {
        let path = write("previous-present.json", "{}")
            .parent()
            .unwrap()
            .join("previous-absent.json");
        let err = load(path.to_str().unwrap()).expect_err("should fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("reading") && msg.contains("previous-absent.json"),
            "expected the path and \"reading\" in the error, got {msg}"
        );
    }

    // The URL has to be resolvable without knowing the previous tag, or the
    // lookup could not run before the tag it is generating for exists.
    #[test]
    fn the_running_target_is_a_known_platform() {
        assert!(
            TARGETS.iter().any(|(t, ..)| *t == TARGET) || TARGET == "unknown",
            "the build target {TARGET} is not in TARGETS, so `snag update` \
             could not find its own asset"
        );
    }

    #[test]
    fn a_tag_url_names_that_release() {
        assert_eq!(
            tag_url("ShortyPing/snag", "v0.1.0"),
            "https://github.com/ShortyPing/snag/releases/download/v0.1.0/revision.json"
        );
    }

    #[test]
    fn the_default_lookup_url_is_version_free() {
        let url = latest_url("ShortyPing/snag");
        assert_eq!(
            url,
            "https://github.com/ShortyPing/snag/releases/latest/download/revision.json"
        );
    }
}
