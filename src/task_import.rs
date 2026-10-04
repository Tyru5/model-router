use crate::config::{MAX_TASK_BYTES, validate_task};
use anyhow::{Context, Result, ensure};
use std::{
    fs::OpenOptions,
    io::Read,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

pub const MAX_DOCUMENT_BYTES: usize = MAX_TASK_BYTES + 3;

/// Read a bounded snapshot of a local Markdown or other UTF-8 text document.
pub fn read(path: &Path) -> Result<String> {
    let mut options = OpenOptions::new();
    options.read(true);
    // Selecting a named pipe or device must not leave the TUI waiting for input.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .with_context(|| format!("Open task/spec: {}", path.display()))?;
    let metadata = file.metadata().context("Read task/spec metadata")?;
    ensure!(metadata.is_file(), "Select a regular Markdown or text file");
    // Allow three extra bytes for an optional UTF-8 byte-order mark.
    let maximum = MAX_DOCUMENT_BYTES;
    ensure!(
        metadata.len() <= maximum as u64,
        "Task/spec exceeds the 32 KiB limit; shorten it or reference larger files in the task"
    );
    let mut bytes = Vec::new();
    file.take((maximum + 1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("Read task/spec: {}", path.display()))?;
    ensure!(bytes.len() <= maximum, "Task/spec exceeds the 32 KiB limit");
    decode(bytes)
}

fn decode(bytes: Vec<u8>) -> Result<String> {
    let text = String::from_utf8(bytes).context(
        "Task/spec must be UTF-8 text; export rich documents to Markdown or plain text first",
    )?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    ensure!(
        !text.starts_with("%PDF-")
            && !text
                .chars()
                .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t')),
        "Task/spec must be a text document; export PDF, Word, or binary files to Markdown or plain text first"
    );
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    validate_task(&text)?;
    Ok(text)
}

#[derive(Debug, PartialEq)]
enum Remote {
    Issue {
        endpoint: String,
        source: String,
    },
    GitHubFile {
        repository: String,
        location: String,
        revision: String,
        kind: Option<String>,
    },
    Text(reqwest::Url),
}

fn remote(input: &str) -> Result<Remote> {
    let mut url =
        reqwest::Url::parse(input.trim()).context("Enter a complete http:// or https:// URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
        "Use an HTTP or HTTPS URL"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "Use a URL without embedded credentials; GitHub imports use gh auth login"
    );
    url.set_fragment(None);
    let parts: Vec<_> = url.path_segments().context("URL has no path")?.collect();
    let host = url.host_str().unwrap_or("");
    if matches!(
        host,
        "github.com" | "www.github.com" | "raw.githubusercontent.com"
    ) {
        ensure!(
            url.port_or_known_default() == Some(if url.scheme() == "https" { 443 } else { 80 }),
            "Use a standard GitHub URL"
        );
        ensure!(
            parts.len() >= 4 && parts.iter().all(|part| !part.is_empty()),
            "Paste a GitHub issue or file URL"
        );
        let owner = parts[0];
        let repo = parts[1];
        let valid_name = |name: &str| {
            name.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        };
        ensure!(
            valid_name(owner) && valid_name(repo),
            "Invalid GitHub repository URL"
        );
        if host != "raw.githubusercontent.com" && parts[2] == "issues" {
            ensure!(
                parts.len() == 4 && parts[3].parse::<u64>().is_ok_and(|n| n > 0),
                "Paste an issue URL ending in /issues/NUMBER"
            );
            return Ok(Remote::Issue {
                endpoint: format!("repos/{owner}/{repo}/issues/{}", parts[3]),
                source: format!("https://github.com/{owner}/{repo}/issues/{}", parts[3]),
            });
        }
        let mut tail = if host == "raw.githubusercontent.com" {
            &parts[2..]
        } else {
            ensure!(
                matches!(parts[2], "blob" | "raw"),
                "Use a GitHub issue, blob, or raw file URL"
            );
            &parts[3..]
        };
        let mut kind = None;
        if tail.len() >= 4 && tail[0] == "refs" && matches!(tail[1], "heads" | "tags") {
            kind = Some(tail[1].to_owned());
            tail = &tail[2..];
        }
        ensure!(
            tail.len() >= 2,
            "GitHub file URL must include a revision and file path"
        );
        let revision = percent_encoding::percent_decode_str(tail[0])
            .decode_utf8()
            .context("Invalid GitHub revision")?;
        return Ok(Remote::GitHubFile {
            repository: format!("{owner}/{repo}"),
            location: percent_encoding::percent_decode_str(&tail.join("/"))
                .decode_utf8()
                .context("Invalid GitHub file path")?
                .into_owned(),
            revision: revision.into_owned(),
            kind,
        });
    }
    Ok(Remote::Text(url))
}

/// Import a remote document without starting a harness or writing to the remote service.
pub fn read_url(input: &str, cancel: &AtomicBool) -> Result<String> {
    ensure!(!cancel.load(Ordering::Relaxed), "Import cancelled");
    match remote(input)? {
        Remote::Issue { endpoint, source } => {
            let json = github(&endpoint, "{title,body}", cancel)?;
            let value: serde_json::Value =
                serde_json::from_slice(&json).context("Read GitHub issue response")?;
            let title = value["title"]
                .as_str()
                .context("GitHub response is missing an issue title")?;
            let body = value["body"].as_str().unwrap_or("");
            decode(format!("# {title}\n\nSource: {source}\n\n{body}").into_bytes())
        }
        Remote::GitHubFile {
            repository,
            location,
            revision,
            kind,
        } => {
            use base64::Engine;
            let endpoint =
                file_endpoint(&repository, &location, &revision, kind.as_deref(), cancel)?;
            let json = github(&endpoint, "{type,size,encoding,content}", cancel)?;
            let value: serde_json::Value =
                serde_json::from_slice(&json).context("Read GitHub file response")?;
            ensure!(
                value["type"] == "file",
                "Select a GitHub file, not a directory"
            );
            ensure!(
                value["size"]
                    .as_u64()
                    .is_some_and(|size| size <= MAX_DOCUMENT_BYTES as u64),
                "Task/spec exceeds the 32 KiB limit"
            );
            ensure!(
                value["encoding"] == "base64",
                "GitHub did not return text file contents"
            );
            let encoded = value["content"]
                .as_str()
                .context("GitHub response is missing file content")?
                .split_whitespace()
                .collect::<String>();
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .context("Decode GitHub file")?;
            decode(bytes)
        }
        Remote::Text(url) => {
            let client = reqwest::blocking::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(20))
                .redirect(reqwest::redirect::Policy::limited(5))
                .user_agent(concat!("model-router/", env!("CARGO_PKG_VERSION")))
                .build()?;
            let response = client
                .get(url)
                .send()
                .context("Fetch task/spec URL")?
                .error_for_status()
                .context("Task/spec URL returned an error")?;
            let kind = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_ascii_lowercase();
            ensure!(
                !kind.contains("text/html")
                    && !kind.contains("application/xhtml")
                    && !kind.contains("application/pdf"),
                "Use a direct Markdown/text URL or a GitHub issue/file URL; web pages and PDFs are not text specs"
            );
            ensure!(
                response
                    .content_length()
                    .is_none_or(|size| size <= MAX_DOCUMENT_BYTES as u64),
                "Task/spec exceeds the 32 KiB limit"
            );
            let mut bytes = Vec::new();
            response
                .take((MAX_DOCUMENT_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .context("Read task/spec response")?;
            ensure!(!cancel.load(Ordering::Relaxed), "Import cancelled");
            ensure!(
                bytes.len() <= MAX_DOCUMENT_BYTES,
                "Task/spec exceeds the 32 KiB limit"
            );
            decode(bytes)
        }
    }
}

fn contents_endpoint(repository: &str, revision: &str, path: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(&format!(
        "https://api.github.com/repos/{repository}/contents/"
    ))?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("Invalid GitHub file URL"))?
        .pop_if_empty()
        .extend(path.split('/'));
    url.query_pairs_mut().append_pair("ref", revision);
    Ok(format!(
        "{}?{}",
        url.path().trim_start_matches('/'),
        url.query().unwrap_or("")
    ))
}

fn file_endpoint(
    repository: &str,
    location: &str,
    revision: &str,
    kind: Option<&str>,
    cancel: &AtomicBool,
) -> Result<String> {
    // GitHub branch names can contain slashes. Resolve the longest matching ref
    // instead of mistaking part of the branch name for a directory.
    if kind.is_some() || !(revision.len() == 40 && revision.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        for candidate in ["heads", "tags"] {
            if kind.is_some_and(|kind| kind != candidate) {
                continue;
            }
            let prefix =
                percent_encoding::utf8_percent_encode(revision, percent_encoding::NON_ALPHANUMERIC);
            let json = github(
                &format!("repos/{repository}/git/matching-refs/{candidate}/{prefix}"),
                "map(.ref)",
                cancel,
            )?;
            let refs: Vec<String> =
                serde_json::from_slice(&json).context("Read GitHub revisions")?;
            if let Some(reference) = matching_revision(location, &refs, candidate) {
                let path = location
                    .strip_prefix(&format!("{reference}/"))
                    .context("GitHub URL must name a file")?;
                return contents_endpoint(
                    repository,
                    &format!("refs/{candidate}/{reference}"),
                    path,
                );
            }
        }
    }
    let path = location
        .strip_prefix(&format!("{revision}/"))
        .context("GitHub URL must name a file")?;
    contents_endpoint(repository, revision, path)
}

fn matching_revision<'a>(location: &str, refs: &'a [String], kind: &str) -> Option<&'a str> {
    let prefix = format!("refs/{kind}/");
    refs.iter()
        .filter_map(|reference| reference.strip_prefix(&prefix))
        .filter(|reference| location.starts_with(&format!("{reference}/")))
        .max_by_key(|reference| reference.len())
}

fn github(endpoint: &str, query: &str, cancel: &AtomicBool) -> Result<Vec<u8>> {
    use crate::{
        adapters::{Invocation, OutputKind},
        process,
    };
    let temp = tempfile::tempdir()?;
    let inv = Invocation {
        program: "gh".into(),
        args: vec![
            "api".into(),
            "--hostname".into(),
            "github.com".into(),
            "--method".into(),
            "GET".into(),
            endpoint.into(),
            "--jq".into(),
            query.into(),
        ],
        stdin: None,
        kind: OutputKind::Text,
        final_file: temp.path().join("unused"),
    };
    let mut received = 0;
    let mut oversized = false;
    let result = process::capture(
        &inv,
        temp.path(),
        temp.path(),
        Duration::from_secs(20),
        cancel,
        |_, chunk| {
            received += chunk.len();
            if received > 4 * MAX_DOCUMENT_BYTES {
                oversized = true;
                cancel.store(true, Ordering::Relaxed);
            }
        },
    )
    .context("Run GitHub CLI; install gh and run gh auth login to import GitHub documents")?;
    ensure!(!oversized, "GitHub document exceeds the import size limit");
    ensure!(!cancel.load(Ordering::Relaxed), "Import cancelled");
    if !result.success() {
        let message = process::read_tail(&temp.path().join("stderr.log"), 2000).unwrap_or_default();
        anyhow::bail!(
            "GitHub import failed or timed out. Check gh auth login, repository access, and the URL. {}",
            crate::storage::clean_terminal(&message)
        );
    }
    std::fs::read(temp.path().join("stdout.log")).context("Read GitHub response")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn recognizes_github_issues_file_links_and_encoded_revisions() {
        assert_eq!(
            remote("https://github.com/acme/project/issues/7#issuecomment-1").unwrap(),
            Remote::Issue {
                endpoint: "repos/acme/project/issues/7".into(),
                source: "https://github.com/acme/project/issues/7".into(),
            }
        );
        for url in [
            "https://github.com/acme/project/blob/feature%2Fnew/docs/spec%20one.md#L2",
            "https://raw.githubusercontent.com/acme/project/feature%2Fnew/docs/spec%20one.md",
            "https://github.com/acme/project/raw/refs/heads/feature%2Fnew/docs/spec%20one.md",
        ] {
            assert_eq!(
                remote(url).unwrap(),
                Remote::GitHubFile {
                    repository: "acme/project".into(),
                    location: "feature/new/docs/spec one.md".into(),
                    revision: "feature/new".into(),
                    kind: url.contains("/refs/heads/").then(|| "heads".into()),
                }
            );
        }
        let refs = vec![
            "refs/heads/feature/new".into(),
            "refs/heads/feature/other".into(),
        ];
        assert_eq!(
            matching_revision("feature/new/docs/spec one.md", &refs, "heads"),
            Some("feature/new")
        );
        assert_eq!(
            matching_revision("feature/newish/spec.md", &refs, "heads"),
            None
        );
        assert_eq!(
            contents_endpoint("acme/project", "refs/heads/feature/new", "docs/spec one.md")
                .unwrap(),
            "repos/acme/project/contents/docs/spec%20one.md?ref=refs%2Fheads%2Ffeature%2Fnew"
        );
        assert!(matches!(
            remote("https://github.com.example.com/spec.md").unwrap(),
            Remote::Text(_)
        ));
        for url in [
            "file:///etc/passwd",
            "https://user:password@example.com/spec.md",
            "https://github.com/acme/project/issues/new",
            "https://github.com/acme/project/tree/main",
        ] {
            assert!(remote(url).is_err(), "{url}");
        }
    }

    #[test]
    fn fetches_direct_text_and_rejects_http_errors_html_and_large_responses() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
            thread,
        };
        for (status, kind, body, valid) in [
            (
                "200 OK",
                "text/markdown",
                "# Remote task 雪\n".as_bytes().to_vec(),
                true,
            ),
            (
                "403 Forbidden",
                "text/plain",
                b"Access denied".to_vec(),
                false,
            ),
            (
                "200 OK",
                "text/html; charset=utf-8",
                b"<html>Sign in</html>".to_vec(),
                false,
            ),
            (
                "200 OK",
                "text/plain",
                vec![b'x'; MAX_DOCUMENT_BYTES + 1],
                false,
            ),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/spec.md", listener.local_addr().unwrap());
            let expected = body.clone();
            let server = thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                let mut bytes = [0; 8192];
                let count = socket.read(&mut bytes).unwrap();
                let request = String::from_utf8_lossy(&bytes[..count]).to_ascii_lowercase();
                assert!(!request.contains("authorization:"));
                let _ = write!(
                    socket,
                    "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(&body);
            });
            let result = read_url(&url, &AtomicBool::new(false));
            server.join().unwrap();
            assert_eq!(result.is_ok(), valid, "{result:?}");
            if valid {
                assert_eq!(result.unwrap().as_bytes(), expected);
            }
        }
        assert!(read_url("https://example.com/spec.md", &AtomicBool::new(true)).is_err());
    }

    #[test]
    fn imports_unicode_and_normalizes_bom_and_windows_line_endings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Feature spec 雪.md");
        fs::write(&path, "\u{feff}# Feature 雪\r\n\r\n- Keep $(literal)\r\n").unwrap();
        assert_eq!(read(&path).unwrap(), "# Feature 雪\n\n- Keep $(literal)\n");
        fs::write(&path, "x".repeat(MAX_TASK_BYTES)).unwrap();
        assert_eq!(read(&path).unwrap().len(), MAX_TASK_BYTES);
    }

    #[test]
    fn rejects_unreadable_empty_binary_and_oversized_documents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spec");
        assert!(read(&path).is_err());
        assert!(read(dir.path()).is_err());
        for bytes in [
            vec![],
            b" \n\t".to_vec(),
            vec![0xff, 0xfe],
            b"text\0binary".to_vec(),
            b"%PDF-1.7\ntext-only PDF container".to_vec(),
            b"\x1b[31mescape".to_vec(),
            vec![b'x'; MAX_TASK_BYTES + 1],
        ] {
            fs::write(&path, bytes).unwrap();
            assert!(read(&path).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn accepts_file_symlinks_and_rejects_pipes_without_waiting() {
        use std::{
            ffi::CString,
            os::unix::{ffi::OsStrExt, fs::symlink},
            time::Instant,
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spec.md");
        fs::write(&path, "# Test task").unwrap();
        let link = dir.path().join("linked.md");
        symlink(&path, &link).unwrap();
        assert_eq!(read(&link).unwrap(), "# Test task");
        let fifo = dir.path().join("pipe");
        let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: name is a valid, NUL-terminated path inside this test's directory.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let started = Instant::now();
        assert!(read(&fifo).is_err());
        assert!(started.elapsed().as_secs() < 1);
    }
}
