use std::io::Write;
use std::time::Duration;

use serde_json::json;

use crate::manifest::Test;
use crate::{Ctx, Format};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Passed,
    Failed,
    TimedOut,
    Skipped,
}

impl Status {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Passed => "passed",
            Status::Failed => "failed",
            Status::TimedOut => "timed_out",
            Status::Skipped => "skipped",
        }
    }

    #[must_use]
    pub fn is_failure(self) -> bool {
        matches!(self, Status::Failed | Status::TimedOut)
    }
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub test: Test,
    pub status: Status,
    pub message: Option<String>,
    pub duration: Duration,
    pub attempts: u32,
    pub output: Vec<String>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Summary {
    pub passed: usize,
    pub failed: usize,
    pub timed_out: usize,
    pub skipped: usize,
    pub duration: Duration,
}

impl Summary {
    #[must_use]
    pub fn total(&self) -> usize {
        self.passed + self.failed + self.timed_out + self.skipped
    }

    #[must_use]
    pub fn is_green(&self) -> bool {
        self.failed == 0 && self.timed_out == 0
    }

    pub fn record(&mut self, status: Status) {
        match status {
            Status::Passed => self.passed += 1,
            Status::Failed => self.failed += 1,
            Status::TimedOut => self.timed_out += 1,
            Status::Skipped => self.skipped += 1,
        }
    }
}

// Streaming formats write per event; json/junit buffer and emit in run_finished.
pub trait Reporter {
    fn run_started(&mut self, _tests: &[Test]) -> anyhow::Result<()> {
        Ok(())
    }
    fn test_started(&mut self, _test: &Test) -> anyhow::Result<()> {
        Ok(())
    }
    fn test_finished(&mut self, _outcome: &Outcome) -> anyhow::Result<()> {
        Ok(())
    }
    fn run_finished(&mut self, _summary: &Summary) -> anyhow::Result<()> {
        Ok(())
    }
}

// Broadcasts events to several reporters at once (used by `--report FILE`).
pub struct Multi(pub Vec<Box<dyn Reporter>>);

impl Reporter for Multi {
    fn run_started(&mut self, tests: &[Test]) -> anyhow::Result<()> {
        self.0.iter_mut().try_for_each(|r| r.run_started(tests))
    }
    fn test_started(&mut self, test: &Test) -> anyhow::Result<()> {
        self.0.iter_mut().try_for_each(|r| r.test_started(test))
    }
    fn test_finished(&mut self, outcome: &Outcome) -> anyhow::Result<()> {
        self.0.iter_mut().try_for_each(|r| r.test_finished(outcome))
    }
    fn run_finished(&mut self, summary: &Summary) -> anyhow::Result<()> {
        self.0.iter_mut().try_for_each(|r| r.run_finished(summary))
    }
}

#[must_use]
pub fn reporter_for(format: Format, ctx: &Ctx, out: Box<dyn Write + Send>) -> Box<dyn Reporter> {
    match format {
        Format::Human => Box::new(Human::new(out, ctx.color, ctx.quiet, ctx.verbose > 0)),
        Format::Jsonl => Box::new(Jsonl { out }),
        Format::Json => Box::new(Json { out, tests: vec![] }),
        Format::Teamcity => Box::new(TeamCity { out }),
        Format::Junit => Box::new(Junit { out, tests: vec![] }),
    }
}

pub struct Human {
    out: Box<dyn Write + Send>,
    color: bool,
    quiet: bool,
    // With -v, print a passing test's captured output too, not just failures'.
    verbose: bool,
    failures: Vec<Outcome>,
}

impl Human {
    fn new(out: Box<dyn Write + Send>, color: bool, quiet: bool, verbose: bool) -> Self {
        Human {
            out,
            color,
            quiet,
            verbose,
            failures: vec![],
        }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
}

impl Reporter for Human {
    fn run_started(&mut self, tests: &[Test]) -> anyhow::Result<()> {
        if !self.quiet {
            let suites = {
                let mut s: Vec<_> = tests.iter().map(|t| &t.suite_path).collect();
                s.sort();
                s.dedup();
                s.len()
            };
            writeln!(
                self.out,
                "running {} test{} across {} suite{}",
                tests.len(),
                plural(tests.len()),
                suites,
                plural(suites)
            )?;
        }
        Ok(())
    }

    fn test_finished(&mut self, outcome: &Outcome) -> anyhow::Result<()> {
        let (mark, code) = match outcome.status {
            Status::Passed => ("PASS", "32"),
            Status::Failed => ("FAIL", "31"),
            Status::TimedOut => ("TIME", "31"),
            Status::Skipped => ("SKIP", "33"),
        };

        if !self.quiet || outcome.status.is_failure() {
            let retries = if outcome.attempts > 1 {
                format!(" (after {} attempts)", outcome.attempts)
            } else {
                String::new()
            };
            writeln!(
                self.out,
                "{} {} {}{}",
                self.paint(code, mark),
                outcome.test.name,
                self.paint("90", &format!("[{}]", fmt_duration(outcome.duration))),
                retries
            )?;
        }

        if self.verbose && !outcome.status.is_failure() {
            for line in &outcome.output {
                writeln!(self.out, "    {} {line}", self.paint("90", "|"))?;
            }
        }

        if outcome.status.is_failure() {
            self.failures.push(outcome.clone());
        }
        Ok(())
    }

    fn run_finished(&mut self, summary: &Summary) -> anyhow::Result<()> {
        if !self.failures.is_empty() {
            writeln!(self.out, "\nfailures:")?;
            for f in &self.failures {
                writeln!(
                    self.out,
                    "\n  {} ({})",
                    self.paint("1;31", &f.test.name),
                    f.test.qualified_id()
                )?;
                if let Some(msg) = &f.message {
                    for line in msg.lines() {
                        writeln!(self.out, "    {line}")?;
                    }
                }
                for line in &f.output {
                    writeln!(self.out, "    {} {line}", self.paint("90", "|"))?;
                }
            }
        }

        let verdict = if summary.is_green() {
            self.paint("32", "ok")
        } else {
            self.paint("31", "FAILED")
        };
        writeln!(
            self.out,
            "\ntest result: {verdict}. {} passed; {} failed; {} timed out; {} skipped; finished in {}",
            summary.passed,
            summary.failed,
            summary.timed_out,
            summary.skipped,
            fmt_duration(summary.duration),
        )?;
        self.out.flush()?;
        Ok(())
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[must_use]
pub fn fmt_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.2}s", d.as_secs_f64())
    }
}

fn outcome_json(o: &Outcome) -> serde_json::Value {
    json!({
        "id": o.test.qualified_id(),
        "test_id": o.test.id,
        "name": o.test.name,
        "suite": o.test.suite_title,
        "suite_path": o.test.suite_path.display().to_string(),
        "script": o.test.script.display().to_string(),
        "tags": o.test.tags,
        "status": o.status.as_str(),
        "duration_ms": o.duration.as_millis() as u64,
        "attempts": o.attempts,
        "message": o.message,
        "output": o.output,
    })
}

pub struct Jsonl {
    out: Box<dyn Write + Send>,
}

impl Reporter for Jsonl {
    fn test_finished(&mut self, outcome: &Outcome) -> anyhow::Result<()> {
        writeln!(self.out, "{}", outcome_json(outcome))?;
        self.out.flush()?;
        Ok(())
    }

    fn run_finished(&mut self, summary: &Summary) -> anyhow::Result<()> {
        writeln!(self.out, "{}", summary_json(summary))?;
        self.out.flush()?;
        Ok(())
    }
}

fn summary_json(s: &Summary) -> serde_json::Value {
    json!({
        "type": "summary",
        "total": s.total(),
        "passed": s.passed,
        "failed": s.failed,
        "timed_out": s.timed_out,
        "skipped": s.skipped,
        "duration_ms": s.duration.as_millis() as u64,
    })
}

pub struct Json {
    out: Box<dyn Write + Send>,
    tests: Vec<serde_json::Value>,
}

impl Reporter for Json {
    fn test_finished(&mut self, outcome: &Outcome) -> anyhow::Result<()> {
        self.tests.push(outcome_json(outcome));
        Ok(())
    }

    fn run_finished(&mut self, summary: &Summary) -> anyhow::Result<()> {
        let doc = json!({
            "schema": "snag.run/v1",
            "summary": summary_json(summary),
            "tests": self.tests,
        });
        writeln!(self.out, "{}", serde_json::to_string_pretty(&doc)?)?;
        self.out.flush()?;
        Ok(())
    }
}

pub struct TeamCity {
    out: Box<dyn Write + Send>,
}

fn tc_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\'' => out.push_str("|'"),
            '\n' => out.push_str("|n"),
            '\r' => out.push_str("|r"),
            '|' => out.push_str("||"),
            '[' => out.push_str("|["),
            ']' => out.push_str("|]"),
            _ => out.push(c),
        }
    }
    out
}

impl Reporter for TeamCity {
    fn run_started(&mut self, tests: &[Test]) -> anyhow::Result<()> {
        let name = tests.first().map_or("snag", |t| t.suite_title.as_str());
        writeln!(
            self.out,
            "##teamcity[testSuiteStarted name='{}']",
            tc_escape(name)
        )?;
        Ok(())
    }

    fn test_started(&mut self, test: &Test) -> anyhow::Result<()> {
        writeln!(
            self.out,
            "##teamcity[testStarted name='{}' locationHint='file://{}']",
            tc_escape(&test.name),
            tc_escape(&test.script.display().to_string())
        )?;
        self.out.flush()?;
        Ok(())
    }

    fn test_finished(&mut self, o: &Outcome) -> anyhow::Result<()> {
        let name = tc_escape(&o.test.name);
        for line in &o.output {
            writeln!(
                self.out,
                "##teamcity[testStdOut name='{name}' out='{}']",
                tc_escape(line)
            )?;
        }
        match o.status {
            Status::Failed | Status::TimedOut => writeln!(
                self.out,
                "##teamcity[testFailed name='{name}' message='{}']",
                tc_escape(o.message.as_deref().unwrap_or("failed"))
            )?,
            Status::Skipped => writeln!(
                self.out,
                "##teamcity[testIgnored name='{name}' message='{}']",
                tc_escape(o.message.as_deref().unwrap_or("skipped"))
            )?,
            Status::Passed => {}
        }
        writeln!(
            self.out,
            "##teamcity[testFinished name='{name}' duration='{}']",
            o.duration.as_millis()
        )?;
        self.out.flush()?;
        Ok(())
    }

    fn run_finished(&mut self, _summary: &Summary) -> anyhow::Result<()> {
        writeln!(self.out, "##teamcity[testSuiteFinished name='snag']")?;
        self.out.flush()?;
        Ok(())
    }
}

pub struct Junit {
    out: Box<dyn Write + Send>,
    tests: Vec<Outcome>,
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // Control chars are illegal in XML 1.0 even when escaped.
            c if (c as u32) < 0x20 && c != '\n' && c != '\t' && c != '\r' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

impl Reporter for Junit {
    fn test_finished(&mut self, outcome: &Outcome) -> anyhow::Result<()> {
        self.tests.push(outcome.clone());
        Ok(())
    }

    fn run_finished(&mut self, summary: &Summary) -> anyhow::Result<()> {
        writeln!(self.out, r#"<?xml version="1.0" encoding="UTF-8"?>"#)?;
        writeln!(
            self.out,
            r#"<testsuites name="snag" tests="{}" failures="{}" skipped="{}" time="{:.3}">"#,
            summary.total(),
            summary.failed + summary.timed_out,
            summary.skipped,
            summary.duration.as_secs_f64()
        )?;
        writeln!(
            self.out,
            r#"  <testsuite name="snag" tests="{}" failures="{}" skipped="{}" time="{:.3}">"#,
            summary.total(),
            summary.failed + summary.timed_out,
            summary.skipped,
            summary.duration.as_secs_f64()
        )?;

        for t in &self.tests {
            writeln!(
                self.out,
                r#"    <testcase classname="{}" name="{}" time="{:.3}">"#,
                xml_escape(&t.test.suite_title),
                xml_escape(&t.test.name),
                t.duration.as_secs_f64()
            )?;
            match t.status {
                Status::Failed | Status::TimedOut => writeln!(
                    self.out,
                    r#"      <failure message="{}" type="{}"/>"#,
                    xml_escape(t.message.as_deref().unwrap_or("failed")),
                    t.status.as_str()
                )?,
                Status::Skipped => writeln!(
                    self.out,
                    r#"      <skipped message="{}"/>"#,
                    xml_escape(t.message.as_deref().unwrap_or("skipped"))
                )?,
                Status::Passed => {}
            }
            if !t.output.is_empty() {
                writeln!(
                    self.out,
                    "      <system-out>{}</system-out>",
                    xml_escape(&t.output.join("\n"))
                )?;
            }
            writeln!(self.out, "    </testcase>")?;
        }

        writeln!(self.out, "  </testsuite>")?;
        writeln!(self.out, "</testsuites>")?;
        self.out.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_escaping_covers_attributes_and_control_chars() {
        assert_eq!(xml_escape("a<b&c\"d'e"), "a&lt;b&amp;c&quot;d&apos;e");
        assert_eq!(xml_escape("bell\u{7}"), "bell ");
    }

    #[test]
    fn teamcity_escaping() {
        assert_eq!(tc_escape("a'b\nc|d[e]"), "a|'b|nc||d|[e|]");
    }

    #[test]
    fn duration_formatting_switches_units() {
        assert_eq!(fmt_duration(Duration::from_millis(999)), "999ms");
        assert_eq!(fmt_duration(Duration::from_millis(1500)), "1.50s");
    }

    #[test]
    fn summary_is_red_when_a_test_times_out() {
        let mut s = Summary::default();
        s.record(Status::Passed);
        s.record(Status::TimedOut);
        assert!(!s.is_green());
        assert_eq!(s.total(), 2);
    }
}
