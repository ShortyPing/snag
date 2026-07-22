use std::path::{Path, PathBuf};

use anyhow::Context;
use regex::RegexSet;

use crate::SelectionArgs;
use crate::manifest::{Test, load_suite};

const IGNORED_DIRS: &[&str] = &["target", "node_modules", "dist", "build", ".venv"];

// The one entry point run/list/check share, so they always see the same tests.
pub fn discover(selection: &SelectionArgs) -> anyhow::Result<Vec<Test>> {
    let suites = suite_files(&selection.paths)?;

    if suites.is_empty() {
        anyhow::bail!(
            "no suite files found (looked for {}). Run `snag init` to create one.",
            describe_search(&selection.paths)
        );
    }

    let mut tests = Vec::new();
    for suite in &suites {
        tests.extend(load_suite(suite)?);
    }

    let filter = Filter::new(selection)?;
    tests.retain(|t| filter.accepts(t));
    Ok(tests)
}

fn describe_search(paths: &[PathBuf]) -> String {
    if paths.is_empty() {
        "suite.toml / snag.toml / *.snag.toml under the current directory".to_string()
    } else {
        paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

// empty -> walk cwd; glob -> expand; dir -> walk; file -> take as-is.
pub fn suite_files(paths: &[PathBuf]) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();

    if paths.is_empty() {
        walk_for_suites(Path::new("."), &mut out);
    } else {
        for path in paths {
            let as_str = path.to_string_lossy();
            if is_glob(&as_str) {
                let matches =
                    glob::glob(&as_str).with_context(|| format!("bad glob pattern `{as_str}`"))?;
                for entry in matches {
                    let entry = entry?;
                    if entry.is_dir() {
                        walk_for_suites(&entry, &mut out);
                    } else if is_suite_file(&entry) {
                        out.push(entry);
                    }
                }
            } else if path.is_dir() {
                walk_for_suites(path, &mut out);
            } else if path.is_file() {
                out.push(path.clone());
            } else {
                anyhow::bail!("path does not exist: {}", path.display());
            }
        }
    }

    out.sort();
    out.dedup();
    Ok(out)
}

fn is_glob(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

// Only applied while walking a dir; a file named on the command line skips it.
fn is_suite_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name == "suite.toml" || name == "snag.toml" || name.ends_with(".snag.toml")
}

fn walk_for_suites(root: &Path, out: &mut Vec<PathBuf>) {
    let walker = walkdir::WalkDir::new(root).follow_links(false).into_iter();
    let entries = walker.filter_entry(|e| {
        if !e.file_type().is_dir() || e.depth() == 0 {
            return true;
        }
        let name = e.file_name().to_string_lossy();
        !name.starts_with('.') && !IGNORED_DIRS.contains(&name.as_ref())
    });

    for entry in entries.flatten() {
        if entry.file_type().is_file() && is_suite_file(entry.path()) {
            out.push(entry.into_path());
        }
    }
}

struct Filter {
    include: Option<Matcher>,
    exclude: Option<Matcher>,
    tags: Vec<String>,
}

impl Filter {
    fn new(selection: &SelectionArgs) -> anyhow::Result<Self> {
        Ok(Filter {
            include: Matcher::new(&selection.filter, selection.regex)?,
            exclude: Matcher::new(&selection.exclude, selection.regex)?,
            tags: selection.tag.clone(),
        })
    }

    fn accepts(&self, test: &Test) -> bool {
        // Match on either the label or the id.
        let haystacks = [test.name.as_str(), test.id.as_str()];

        if let Some(include) = &self.include
            && !haystacks.iter().any(|h| include.matches(h))
        {
            return false;
        }

        if let Some(exclude) = &self.exclude
            && haystacks.iter().any(|h| exclude.matches(h))
        {
            return false;
        }

        if !self.tags.is_empty() && !self.tags.iter().any(|t| test.tags.contains(t)) {
            return false;
        }

        true
    }
}

enum Matcher {
    Substrings(Vec<String>),
    Regexes(RegexSet),
}

impl Matcher {
    fn new(patterns: &[String], regex: bool) -> anyhow::Result<Option<Self>> {
        if patterns.is_empty() {
            return Ok(None);
        }
        Ok(Some(if regex {
            Matcher::Regexes(
                RegexSet::new(patterns).context("invalid regular expression in filter")?,
            )
        } else {
            Matcher::Substrings(patterns.to_vec())
        }))
    }

    fn matches(&self, haystack: &str) -> bool {
        match self {
            Matcher::Substrings(subs) => subs.iter().any(|s| haystack.contains(s.as_str())),
            Matcher::Regexes(set) => set.is_match(haystack),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn test_named(id: &str, name: &str, tags: &[&str]) -> Test {
        Test {
            id: id.into(),
            name: name.into(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            timeout: None,
            parallel_safe: true,
            script: PathBuf::from("t.snag"),
            vars: BTreeMap::new(),
            suite_title: "s".into(),
            suite_path: PathBuf::from("suite.toml"),
        }
    }

    fn selection(filter: &[&str], exclude: &[&str], tag: &[&str], regex: bool) -> SelectionArgs {
        SelectionArgs {
            paths: vec![],
            filter: filter.iter().map(|s| s.to_string()).collect(),
            tag: tag.iter().map(|s| s.to_string()).collect(),
            exclude: exclude.iter().map(|s| s.to_string()).collect(),
            regex,
        }
    }

    #[test]
    fn substring_filter_matches_name_or_id() {
        let f = Filter::new(&selection(&["goog"], &[], &[], false)).unwrap();
        assert!(f.accepts(&test_named("google-works", "Check Google", &[])));
        assert!(f.accepts(&test_named("x", "the google test", &[])));
        assert!(!f.accepts(&test_named("bing", "Check Bing", &[])));
    }

    #[test]
    fn exclude_beats_include() {
        let f = Filter::new(&selection(&["check"], &["bing"], &[], false)).unwrap();
        assert!(f.accepts(&test_named("a", "check google", &[])));
        assert!(!f.accepts(&test_named("b", "check bing", &[])));
    }

    #[test]
    fn tags_are_or_ed() {
        let f = Filter::new(&selection(&[], &[], &["smoke", "slow"], false)).unwrap();
        assert!(f.accepts(&test_named("a", "a", &["smoke"])));
        assert!(f.accepts(&test_named("b", "b", &["slow", "network"])));
        assert!(!f.accepts(&test_named("c", "c", &["network"])));
    }

    #[test]
    fn regex_mode_anchors_work() {
        let f = Filter::new(&selection(&["^goo.*works$"], &[], &[], true)).unwrap();
        assert!(f.accepts(&test_named("google-works", "google-works", &[])));
        assert!(!f.accepts(&test_named("x", "not-google-works-either", &[])));
    }

    #[test]
    fn bad_regex_is_an_error() {
        assert!(Filter::new(&selection(&["("], &[], &[], true)).is_err());
    }

    #[test]
    fn suite_name_recognition() {
        assert!(is_suite_file(Path::new("a/suite.toml")));
        assert!(is_suite_file(Path::new("snag.toml")));
        assert!(is_suite_file(Path::new("api.snag.toml")));
        assert!(!is_suite_file(Path::new("Cargo.toml")));
    }
}
