use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    #[serde(default = "default_title")]
    pub title: String,

    #[serde(default)]
    pub variables: BTreeMap<String, String>,

    #[serde(default, with = "humantime_serde")]
    pub timeout: Option<Duration>,

    #[serde(default)]
    pub setup: Option<HookSpec>,

    #[serde(default)]
    pub teardown: Option<HookSpec>,

    #[serde(default, rename = "test")]
    pub tests: Vec<TestDefinition>,
}

// Setup/teardown as written in the TOML: one script, or a list of them, each
// either a bare path or a table with options.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum HookSpec {
    One(HookEntry),
    Many(Vec<HookEntry>),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum HookEntry {
    Path(PathBuf),
    Table(HookTable),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct HookTable {
    pub file: PathBuf,

    // Teardown only: run the script even when the test itself failed.
    pub always: Option<bool>,
}

impl HookSpec {
    fn entries(&self) -> &[HookEntry] {
        match self {
            HookSpec::One(entry) => std::slice::from_ref(entry),
            HookSpec::Many(entries) => entries,
        }
    }
}

// A hook with its path resolved, ready for the runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hook {
    pub script: PathBuf,
    // Teardown: run even after a failed test. Always true for setup.
    pub always: bool,
}

fn default_title() -> String {
    "Untitled suite".to_string()
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct TestDefinition {
    pub id: String,

    pub name: Option<String>,

    pub parallel_safe: Option<bool>,

    #[serde(default)]
    pub tags: Vec<String>,

    #[serde(default, with = "humantime_serde")]
    pub timeout: Option<Duration>,

    pub file: PathBuf,

    #[serde(default)]
    pub variables: BTreeMap<String, String>,

    #[serde(default)]
    pub setup: Option<HookSpec>,

    #[serde(default)]
    pub teardown: Option<HookSpec>,
}

// A TestDefinition with its paths resolved and variables merged, so the runner
// never has to look back at the manifest.
#[derive(Debug, Clone)]
pub struct Test {
    pub id: String,
    pub name: String,
    pub tags: Vec<String>,
    pub timeout: Option<Duration>,
    pub parallel_safe: bool,
    pub script: PathBuf,
    pub vars: BTreeMap<String, String>,
    // Suite hooks first, then the test's own; teardown runs in reverse.
    pub setup: Vec<Hook>,
    pub teardown: Vec<Hook>,
    pub suite_title: String,
    pub suite_path: PathBuf,
}

impl Test {
    pub fn qualified_id(&self) -> String {
        format!("{}::{}", self.suite_path.display(), self.id)
    }
}

pub fn load_suite(path: &Path) -> anyhow::Result<Vec<Test>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading suite {}", path.display()))?;

    let manifest: Manifest =
        toml::from_str(&text).with_context(|| format!("parsing suite {}", path.display()))?;

    let base = path.parent().unwrap_or_else(|| Path::new("."));

    let suite_setup = resolve_hooks(manifest.setup.as_ref(), base, Phase::Setup, path)?;
    let suite_teardown = resolve_hooks(manifest.teardown.as_ref(), base, Phase::Teardown, path)?;

    let mut seen: BTreeMap<&str, ()> = BTreeMap::new();
    let mut tests = Vec::with_capacity(manifest.tests.len());

    for def in &manifest.tests {
        if seen.insert(def.id.as_str(), ()).is_some() {
            anyhow::bail!("duplicate test id `{}` in {}", def.id, path.display());
        }

        let mut vars = manifest.variables.clone();
        vars.extend(def.variables.clone());

        // Suite setup wraps the test's own: outermost first on the way in,
        // and the runner unwinds teardown in reverse.
        let mut setup = suite_setup.clone();
        setup.extend(resolve_hooks(def.setup.as_ref(), base, Phase::Setup, path)?);

        let mut teardown = resolve_hooks(def.teardown.as_ref(), base, Phase::Teardown, path)?;
        teardown.extend(suite_teardown.clone());

        tests.push(Test {
            id: def.id.clone(),
            name: def.name.clone().unwrap_or_else(|| def.id.clone()),
            tags: def.tags.clone(),
            timeout: def.timeout.or(manifest.timeout),
            parallel_safe: def.parallel_safe.unwrap_or(true),
            script: normalize(&base.join(&def.file)),
            vars,
            setup,
            teardown,
            suite_title: manifest.title.clone(),
            suite_path: path.to_path_buf(),
        });
    }

    Ok(tests)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Setup,
    Teardown,
}

fn resolve_hooks(
    spec: Option<&HookSpec>,
    base: &Path,
    phase: Phase,
    suite: &Path,
) -> anyhow::Result<Vec<Hook>> {
    let Some(spec) = spec else {
        return Ok(Vec::new());
    };

    spec.entries()
        .iter()
        .map(|entry| {
            let (file, always) = match entry {
                HookEntry::Path(file) => (file, None),
                HookEntry::Table(table) => (&table.file, table.always),
            };

            // `always` only means something for teardown; silently ignoring it
            // on setup would look like it did something.
            if always.is_some() && phase == Phase::Setup {
                anyhow::bail!(
                    "`always` is only valid on teardown, not setup ({})",
                    suite.display()
                );
            }

            Ok(Hook {
                script: normalize(&base.join(file)),
                always: always.unwrap_or(true),
            })
        })
        .collect()
}

// Drop `.` components so paths print as `tests/api.snag`, not `./tests/./api.snag`.
// Lexical only: `..` is left alone since resolving it can break through symlinks.
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;

    let cleaned: PathBuf = path
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect();

    if cleaned.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("snag-manifest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        path
    }

    #[test]
    fn test_vars_override_suite_vars() {
        let path = write_temp(
            "override.toml",
            r#"
title = "s"
[variables]
base_url = "https://suite"
shared = "keep"

[[test]]
id = "t"
file = "t.snag"
[test.variables]
base_url = "https://test"
"#,
        );
        let tests = load_suite(&path).unwrap();
        assert_eq!(tests[0].vars["base_url"], "https://test");
        assert_eq!(tests[0].vars["shared"], "keep");
    }

    #[test]
    fn defaults_fill_in() {
        let path = write_temp(
            "minimal.toml",
            r#"
[[test]]
id = "t"
file = "t.snag"
"#,
        );
        let tests = load_suite(&path).unwrap();
        assert_eq!(tests[0].name, "t");
        assert!(tests[0].tags.is_empty());
        assert!(tests[0].parallel_safe);
        assert_eq!(tests[0].suite_title, "Untitled suite");
    }

    #[test]
    fn script_path_is_manifest_relative() {
        let path = write_temp(
            "rel.toml",
            r#"
[[test]]
id = "t"
file = "./scripts/t.snag"
"#,
        );
        let tests = load_suite(&path).unwrap();
        assert!(tests[0].script.ends_with("scripts/t.snag"));
    }

    #[test]
    fn normalize_strips_dot_components() {
        assert_eq!(
            normalize(Path::new("./a/./b.snag")),
            PathBuf::from("a/b.snag")
        );
        assert_eq!(normalize(Path::new("./")), PathBuf::from("."));
        assert_eq!(normalize(Path::new("a/../b")), PathBuf::from("a/../b"));
    }

    #[test]
    fn suite_hooks_wrap_test_hooks() {
        let path = write_temp(
            "hooks.toml",
            r#"
setup = "./suite_setup.snag"
teardown = "./suite_teardown.snag"

[[test]]
id = "t"
file = "t.snag"
setup = ["./a.snag", "./b.snag"]
teardown = { file = "./t_teardown.snag", always = false }
"#,
        );
        let tests = load_suite(&path).unwrap();
        let setup: Vec<_> = tests[0]
            .setup
            .iter()
            .map(|h| h.script.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(setup, ["suite_setup.snag", "a.snag", "b.snag"]);

        // Teardown unwinds inward-out: the test's own first, the suite's last.
        let teardown: Vec<_> = tests[0]
            .teardown
            .iter()
            .map(|h| {
                (
                    h.script.file_name().unwrap().to_string_lossy().into_owned(),
                    h.always,
                )
            })
            .collect();
        assert_eq!(
            teardown,
            [
                ("t_teardown.snag".to_string(), false),
                ("suite_teardown.snag".to_string(), true),
            ]
        );
    }

    #[test]
    fn hooks_default_to_always_and_are_manifest_relative() {
        let path = write_temp(
            "hooks_default.toml",
            r#"
[[test]]
id = "t"
file = "t.snag"
teardown = "./scripts/clean.snag"
"#,
        );
        let tests = load_suite(&path).unwrap();
        assert!(tests[0].teardown[0].always);
        assert!(tests[0].teardown[0].script.ends_with("scripts/clean.snag"));
    }

    #[test]
    fn always_on_setup_is_rejected() {
        let path = write_temp(
            "hooks_bad.toml",
            r#"
[[test]]
id = "t"
file = "t.snag"
setup = { file = "./s.snag", always = false }
"#,
        );
        let err = load_suite(&path).unwrap_err().to_string();
        assert!(err.contains("only valid on teardown"), "{err}");
    }

    #[test]
    fn tests_without_hooks_have_none() {
        let path = write_temp(
            "hooks_absent.toml",
            r#"
[[test]]
id = "t"
file = "t.snag"
"#,
        );
        let tests = load_suite(&path).unwrap();
        assert!(tests[0].setup.is_empty());
        assert!(tests[0].teardown.is_empty());
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let path = write_temp(
            "dup.toml",
            r#"
[[test]]
id = "t"
file = "a.snag"

[[test]]
id = "t"
file = "b.snag"
"#,
        );
        assert!(load_suite(&path).is_err());
    }

    #[test]
    fn test_timeout_beats_file_timeout() {
        let path = write_temp(
            "timeouts.toml",
            r#"
timeout = "30s"

[[test]]
id = "a"
file = "a.snag"

[[test]]
id = "b"
file = "b.snag"
timeout = "1s"
"#,
        );
        let tests = load_suite(&path).unwrap();
        assert_eq!(tests[0].timeout, Some(Duration::from_secs(30)));
        assert_eq!(tests[1].timeout, Some(Duration::from_secs(1)));
    }
}
