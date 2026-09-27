#![allow(clippy::disallowed_methods, reason = "tooling is exempt")]

use anyhow::{Context as _, Result, anyhow, bail};
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

pub fn run(repository: &Path, arguments: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(arguments)
        .output()
        .with_context(|| format!("running git {}", arguments.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8(output.stdout).context("git output is not UTF-8")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedFile {
    /// `None` for files the fork added.
    pub base_path: Option<String>,
    pub head_path: String,
}

pub fn changed_rust_files(
    repository: &Path,
    base: &str,
    head: &str,
    pathspec: &str,
) -> Result<Vec<ChangedFile>> {
    let output = run(
        repository,
        &[
            "diff",
            "--name-status",
            "-z",
            "-M",
            "--diff-filter=AMR",
            base,
            head,
            "--",
            pathspec,
        ],
    )?;
    let mut fields = output.split('\0').filter(|field| !field.is_empty());
    let mut files = Vec::new();
    while let Some(status) = fields.next() {
        let file = match status.chars().next() {
            Some('A') => ChangedFile {
                base_path: None,
                head_path: fields.next().context("missing path")?.to_string(),
            },
            Some('M') => {
                let path = fields.next().context("missing path")?.to_string();
                ChangedFile {
                    base_path: Some(path.clone()),
                    head_path: path,
                }
            }
            Some('R') => {
                let old = fields.next().context("missing old path")?.to_string();
                let new = fields.next().context("missing new path")?.to_string();
                ChangedFile {
                    base_path: Some(old),
                    head_path: new,
                }
            }
            _ => bail!("unexpected diff status {status:?}"),
        };
        if file.head_path.ends_with(".rs") {
            files.push(file);
        }
    }
    Ok(files)
}

/// Reads blobs through one long-lived `git cat-file --batch`.
pub struct BlobReader {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl BlobReader {
    pub fn new(repository: &Path) -> Result<Self> {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .context("starting git cat-file --batch")?;
        let stdin = child.stdin.take().context("no stdin for git cat-file")?;
        let stdout = child.stdout.take().context("no stdout for git cat-file")?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    pub fn read(&mut self, revision: &str, path: &str) -> Result<String> {
        writeln!(self.stdin, "{revision}:{path}")?;
        self.stdin.flush()?;
        let mut header = String::new();
        self.stdout.read_line(&mut header)?;
        let header = header.trim_end();
        if header.ends_with("missing") {
            bail!("{revision}:{path} is missing");
        }
        let size: usize = header
            .rsplit(' ')
            .next()
            .and_then(|size| size.parse().ok())
            .ok_or_else(|| anyhow!("unexpected cat-file header {header:?}"))?;
        let mut content = vec![0u8; size + 1];
        self.stdout.read_exact(&mut content)?;
        content.truncate(size);
        Ok(String::from_utf8_lossy(&content).into_owned())
    }
}

impl Drop for BlobReader {
    fn drop(&mut self) {
        if let Err(error) = self.child.kill() {
            eprintln!("failed to stop git cat-file: {error}");
        }
        if let Err(error) = self.child.wait() {
            eprintln!("failed to wait for git cat-file: {error}");
        }
    }
}

/// Files that are compiled only for tests because another file declares them with
/// `#[cfg(test)] mod name;`.
pub fn out_of_line_test_modules(repository: &Path, head: &str) -> Result<Vec<String>> {
    let output = run(
        repository,
        &[
            "grep",
            "-n",
            "-B3",
            "-E",
            r"^\s*(pub(\([^)]*\))?\s+)?mod\s+[A-Za-z_][A-Za-z0-9_]*\s*;",
            head,
            "--",
            "crates/*.rs",
        ],
    );
    // `git grep` exits with 1 when nothing matches, so a failure is reported but not fatal.
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            eprintln!("warning: looking for out-of-line test modules failed: {error:#}");
            String::new()
        }
    };
    let mut result = Vec::new();
    let mut context: Vec<String> = Vec::new();
    for line in output.lines() {
        if line == "--" {
            context.clear();
            continue;
        }
        let Some(rest) = line.strip_prefix(&format!("{head}:")) else {
            continue;
        };
        let (path, separator_and_text) = split_grep_line(rest);
        let Some((is_match, text)) = separator_and_text else {
            continue;
        };
        if !is_match {
            context.push(text.to_string());
            continue;
        }
        let gated = context.iter().rev().take(3).any(|previous| {
            let previous = previous.trim();
            previous.starts_with("#[cfg(") && is_test_cfg_text(previous)
        });
        context.clear();
        if !gated {
            continue;
        }
        if let Some(name) = module_name(text) {
            result.extend(module_file_candidates(&path, &name));
        }
    }
    Ok(result)
}

/// Splits `path:12:text` / `path-12-text` (grep's match / context separators).
fn split_grep_line(rest: &str) -> (String, Option<(bool, &str)>) {
    let Some(rs_end) = rest.find(".rs") else {
        return (String::new(), None);
    };
    let path = rest[..rs_end + 3].to_string();
    let after = &rest[rs_end + 3..];
    let Some(separator) = after.chars().next() else {
        return (path, None);
    };
    let after = &after[1..];
    let Some(second) = after.find(|character: char| !character.is_ascii_digit()) else {
        return (path, None);
    };
    let text = &after[second + 1..];
    (path, Some((separator == ':', text)))
}

fn is_test_cfg_text(attribute: &str) -> bool {
    let words: Vec<&str> = attribute
        .split(|character: char| {
            !(character.is_alphanumeric()
                || character == '_'
                || character == '"'
                || character == '-')
        })
        .filter(|word| !word.is_empty())
        .collect();
    words.contains(&"test") && !words.contains(&"not")
}

fn module_name(line: &str) -> Option<String> {
    let before_semicolon = line.split(';').next()?;
    let name = before_semicolon.split_whitespace().last()?;
    Some(name.to_string())
}

fn module_file_candidates(declaring_path: &str, module: &str) -> Vec<String> {
    let declaring = PathBuf::from(declaring_path);
    let Some(directory) = declaring.parent() else {
        return Vec::new();
    };
    let file_name = declaring
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let stem = declaring
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default();
    let module_directory = if matches!(file_name, "lib.rs" | "main.rs" | "mod.rs") {
        directory.to_path_buf()
    } else {
        directory.join(stem)
    };
    // The crate root may be named after the crate (`[lib] path = "src/foo.rs"`), so both layouts
    // are listed; a path that does not exist is harmless.
    let mut candidates = vec![
        module_directory.join(format!("{module}.rs")),
        module_directory.join(module).join("mod.rs"),
        directory.join(format!("{module}.rs")),
    ];
    candidates.dedup();
    candidates
        .into_iter()
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn grep_line_splitting() {
        assert_eq!(
            split_grep_line("crates/a/src/b.rs:12:mod tests;"),
            ("crates/a/src/b.rs".to_string(), Some((true, "mod tests;")))
        );
        assert_eq!(
            split_grep_line("crates/a/src/b.rs-11-#[cfg(test)]"),
            (
                "crates/a/src/b.rs".to_string(),
                Some((false, "#[cfg(test)]"))
            )
        );
    }

    #[test]
    fn test_cfg_detection() {
        assert!(is_test_cfg_text("#[cfg(test)]"));
        assert!(is_test_cfg_text(
            "#[cfg(any(test, feature = \"test-support\"))]"
        ));
        assert!(!is_test_cfg_text("#[cfg(not(test))]"));
        assert!(!is_test_cfg_text("#[cfg(feature = \"test-support\")]"));
    }
}
