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

    #[serde(default, rename = "test")]
    pub tests: Vec<TestDefinition>,
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

    let mut seen: BTreeMap<&str, ()> = BTreeMap::new();
    let mut tests = Vec::with_capacity(manifest.tests.len());

    for def in &manifest.tests {
        if seen.insert(def.id.as_str(), ()).is_some() {
            anyhow::bail!("duplicate test id `{}` in {}", def.id, path.display());
        }

        let mut vars = manifest.variables.clone();
        vars.extend(def.variables.clone());

        tests.push(Test {
            id: def.id.clone(),
            name: def.name.clone().unwrap_or_else(|| def.id.clone()),
            tags: def.tags.clone(),
            timeout: def.timeout.or(manifest.timeout),
            parallel_safe: def.parallel_safe.unwrap_or(true),
            script: normalize(&base.join(&def.file)),
            vars,
            suite_title: manifest.title.clone(),
            suite_path: path.to_path_buf(),
        });
    }

    Ok(tests)
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
