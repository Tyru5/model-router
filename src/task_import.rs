use crate::config::{MAX_TASK_BYTES, validate_task};
use anyhow::{Context, Result, ensure};
use std::{
    fs::OpenOptions,
    io::Read,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
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
    /// An issue or pull request: its title and body.
    Issue {
        /// GitHub host for `gh api --hostname`: github.com or an Enterprise host.
        host: String,
        endpoint: String,
        source: String,
    },
    GitHubFile {
        host: String,
        repository: String,
        location: String,
        revision: String,
        kind: Option<String>,
    },
    Text(reqwest::Url),
}

/// GitHub Enterprise hosts `gh` is logged in to (its hosts.yml, and GH_HOST).
fn enterprise_hosts() -> Vec<String> {
    let dir = std::env::var_os("GH_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(|d| PathBuf::from(d).join("gh")))
        .or_else(|| directories::BaseDirs::new().map(|d| d.home_dir().join(".config/gh")));
    let mut hosts: Vec<String> = dir
        .and_then(|dir| std::fs::read_to_string(dir.join("hosts.yml")).ok())
        .map(|text| hosts_from_yaml(&text))
        .unwrap_or_default();
    if let Ok(host) = std::env::var("GH_HOST") {
        hosts.push(host.trim().to_ascii_lowercase());
    }
    hosts.retain(|h| !h.is_empty() && h != "github.com");
    hosts
}

/// Top-level keys of gh's hosts.yml, which are host names.
fn hosts_from_yaml(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| !line.starts_with([' ', '\t', '#', '-']))
        .filter_map(|line| line.trim_end().strip_suffix(':'))
        .map(|host| host.trim_matches(['"', '\'']).to_ascii_lowercase())
        .filter(|host| !host.is_empty() && !host.contains(char::is_whitespace))
        .collect()
}

fn remote(input: &str) -> Result<Remote> {
    remote_with(input, &enterprise_hosts())
}

fn remote_with(input: &str, enterprise: &[String]) -> Result<Remote> {
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
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let host = host.as_str();
    let public = matches!(
        host,
        "github.com" | "www.github.com" | "raw.githubusercontent.com"
    );
    let enterprise = !public && url.scheme() == "https" && enterprise.iter().any(|h| h == host);
    if public || enterprise {
        let api_host = if public { "github.com" } else { host }.to_owned();
        ensure!(
            url.port_or_known_default() == Some(if url.scheme() == "https" { 443 } else { 80 }),
            "Use a standard GitHub URL"
        );
        ensure!(
            parts.len() >= 4 && parts.iter().all(|part| !part.is_empty()),
            "Paste a GitHub issue, pull request, or file URL"
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
        let web = if public { "github.com" } else { host };
        if host != "raw.githubusercontent.com" && parts[2] == "issues" {
            ensure!(
                parts.len() == 4 && parts[3].parse::<u64>().is_ok_and(|n| n > 0),
                "Paste an issue URL ending in /issues/NUMBER"
            );
            return Ok(Remote::Issue {
                host: api_host,
                endpoint: format!("repos/{owner}/{repo}/issues/{}", parts[3]),
                source: format!("https://{web}/{owner}/{repo}/issues/{}", parts[3]),
            });
        }
        // Pull requests, including their /files or /commits tabs.
        if host != "raw.githubusercontent.com" && parts[2] == "pull" {
            ensure!(
                parts[3].parse::<u64>().is_ok_and(|n| n > 0),
                "Paste a pull request URL with /pull/NUMBER"
            );
            return Ok(Remote::Issue {
                host: api_host,
                endpoint: format!("repos/{owner}/{repo}/pulls/{}", parts[3]),
                source: format!("https://{web}/{owner}/{repo}/pull/{}", parts[3]),
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
            host: api_host,
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
        Remote::Issue {
            host,
            endpoint,
            source,
        } => {
            let json = github(&host, &endpoint, "{title,body}", cancel)?;
            let value: serde_json::Value =
                serde_json::from_slice(&json).context("Read GitHub issue response")?;
            let title = value["title"]
                .as_str()
                .context("GitHub response is missing a title")?;
            let body = value["body"].as_str().unwrap_or("");
            decode(format!("# {title}\n\nSource: {source}\n\n{body}").into_bytes())
        }
        Remote::GitHubFile {
            host,
            repository,
            location,
            revision,
            kind,
        } => {
            use base64::Engine;
            let endpoint = file_endpoint(
                &host,
                &repository,
                &location,
                &revision,
                kind.as_deref(),
                cancel,
            )?;
            let json = github(&host, &endpoint, "{type,size,encoding,content}", cancel)?;
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
            // Private addresses are reachable only when the URL itself names
            // one; redirects and DNS answers cannot lead there otherwise.
            let allow_private = targets_private(&url);
            let client = reqwest::blocking::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(20))
                .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                    let previous = attempt.previous().last().cloned();
                    if attempt.previous().len() > 5 {
                        return attempt.error("Too many redirects");
                    }
                    match previous.map(|from| redirect_allowed(&from, attempt.url(), allow_private))
                    {
                        Some(Err(error)) => attempt.error(error),
                        _ => attempt.follow(),
                    }
                }))
                .dns_resolver(Arc::new(PublicResolver { allow_private }))
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

/// Loopback, link-local, private (RFC 1918, IPv6 ULA), and unspecified
/// addresses: cloud metadata and local services live there.
fn private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            ip.is_loopback() || ip.is_link_local() || ip.is_private() || ip.is_unspecified()
        }
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return private(IpAddr::V4(v4));
            }
            let first = ip.segments()[0];
            ip.is_loopback()
                || ip.is_unspecified()
                || first & 0xffc0 == 0xfe80
                || first & 0xfe00 == 0xfc00
        }
    }
}

/// The URL's host when it is an IP address literal.
fn host_ip(url: &reqwest::Url) -> Option<IpAddr> {
    url.host_str()?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()
}

/// Whether the URL's host is, or resolves to, a private address.
fn targets_private(url: &reqwest::Url) -> bool {
    let Some(name) = url.host_str() else {
        return false;
    };
    match host_ip(url) {
        Some(ip) => private(ip),
        None => {
            name.eq_ignore_ascii_case("localhost")
                || (name, url.port_or_known_default().unwrap_or(443))
                    .to_socket_addrs()
                    .is_ok_and(|mut addrs| addrs.any(|a| private(a.ip())))
        }
    }
}

/// A redirect may not downgrade HTTPS to HTTP, leave HTTP(S), or reach a
/// private address literal unless the original URL was private.
fn redirect_allowed(
    from: &reqwest::Url,
    to: &reqwest::Url,
    allow_private: bool,
) -> std::result::Result<(), String> {
    if !matches!(to.scheme(), "http" | "https") {
        return Err(format!("Refusing redirect to {} URL", to.scheme()));
    }
    if from.scheme() == "https" && to.scheme() == "http" {
        return Err("Refusing redirect from HTTPS to HTTP".into());
    }
    let literal = match (host_ip(to), to.host_str()) {
        (Some(ip), _) => private(ip),
        (None, Some(name)) => name.eq_ignore_ascii_case("localhost"),
        (None, None) => true,
    };
    if literal && !allow_private {
        return Err(format!(
            "Refusing redirect to private address {}",
            to.host_str().unwrap_or("")
        ));
    }
    Ok(())
}

/// Public addresses of `addrs`, or all of them when private ones are allowed.
fn public_addrs(
    name: &str,
    addrs: Vec<SocketAddr>,
    allow_private: bool,
) -> std::result::Result<Vec<SocketAddr>, String> {
    let kept: Vec<SocketAddr> = addrs
        .into_iter()
        .filter(|a| allow_private || !private(a.ip()))
        .collect();
    if kept.is_empty() {
        return Err(format!(
            "Refusing to connect to {name}: it has no public address"
        ));
    }
    Ok(kept)
}

/// Resolves with the system resolver and drops private addresses, so a
/// public name (or a redirect to one) cannot reach internal services.
struct PublicResolver {
    allow_private: bool,
}
impl reqwest::dns::Resolve for PublicResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let allow_private = self.allow_private;
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = (host.as_str(), 0).to_socket_addrs()?.collect();
            let kept = public_addrs(&host, addrs, allow_private)?;
            Ok(Box::new(kept.into_iter()) as reqwest::dns::Addrs)
        })
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
    host: &str,
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
                host,
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

fn github(host: &str, endpoint: &str, query: &str, cancel: &AtomicBool) -> Result<Vec<u8>> {
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
            host.into(),
            "--method".into(),
            "GET".into(),
            endpoint.into(),
            "--jq".into(),
            query.into(),
        ],
        stdin: None,
        kind: OutputKind::Text,
        final_file: temp.path().join("unused"),
        env: vec![],
        files: vec![],
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
                host: "github.com".into(),
                endpoint: "repos/acme/project/issues/7".into(),
                source: "https://github.com/acme/project/issues/7".into(),
            }
        );
        for url in [
            "https://github.com/acme/project/pull/12",
            "https://github.com/acme/project/pull/12/files",
        ] {
            assert_eq!(
                remote(url).unwrap(),
                Remote::Issue {
                    host: "github.com".into(),
                    endpoint: "repos/acme/project/pulls/12".into(),
                    source: "https://github.com/acme/project/pull/12".into(),
                }
            );
        }
        assert!(remote("https://github.com/acme/project/pull/new").is_err());
        for url in [
            "https://github.com/acme/project/blob/feature%2Fnew/docs/spec%20one.md#L2",
            "https://raw.githubusercontent.com/acme/project/feature%2Fnew/docs/spec%20one.md",
            "https://github.com/acme/project/raw/refs/heads/feature%2Fnew/docs/spec%20one.md",
        ] {
            assert_eq!(
                remote(url).unwrap(),
                Remote::GitHubFile {
                    host: "github.com".into(),
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
    fn enterprise_hosts_use_gh_with_their_hostname() {
        let hosts = hosts_from_yaml(
            "github.com:\n    user: me\nghe.corp.example:\n    git_protocol: https\n    users:\n        me:\n\"Git.Other.Example\":\n",
        );
        assert_eq!(
            hosts,
            ["github.com", "ghe.corp.example", "git.other.example"]
        );
        let enterprise = ["ghe.corp.example".to_owned()];
        assert_eq!(
            remote_with("https://ghe.corp.example/team/app/issues/3", &enterprise).unwrap(),
            Remote::Issue {
                host: "ghe.corp.example".into(),
                endpoint: "repos/team/app/issues/3".into(),
                source: "https://ghe.corp.example/team/app/issues/3".into(),
            }
        );
        assert!(matches!(
            remote_with("https://ghe.corp.example/team/app/blob/main/spec.md", &enterprise)
                .unwrap(),
            Remote::GitHubFile { host, .. } if host == "ghe.corp.example"
        ));
        // Unknown hosts and plain HTTP stay direct text downloads.
        for url in [
            "https://other.example/team/app/issues/3",
            "http://ghe.corp.example/team/app/issues/3",
        ] {
            assert!(
                matches!(remote_with(url, &enterprise).unwrap(), Remote::Text(_)),
                "{url}"
            );
        }
    }

    #[test]
    fn redirects_and_dns_cannot_reach_private_addresses() {
        let url = |text: &str| reqwest::Url::parse(text).unwrap();
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "::1",
            "fe80::1",
            "fd00::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(private(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "172.32.0.1", "2606:4700::1"] {
            assert!(!private(ip.parse().unwrap()), "{ip}");
        }
        let public = url("https://docs.example/spec.md");
        assert!(redirect_allowed(&public, &url("https://cdn.example/spec.md"), false).is_ok());
        assert!(redirect_allowed(&public, &url("http://cdn.example/spec.md"), false).is_err());
        assert!(redirect_allowed(&public, &url("ftp://cdn.example/spec.md"), false).is_err());
        for target in [
            "https://169.254.169.254/latest/meta-data",
            "https://127.0.0.1:8080/",
            "https://[::1]/",
            "https://localhost/",
        ] {
            assert!(
                redirect_allowed(&public, &url(target), false).is_err(),
                "{target}"
            );
            assert!(
                redirect_allowed(&public, &url(target), true).is_ok(),
                "{target}"
            );
        }
        assert!(
            redirect_allowed(&url("http://a.example/"), &url("http://b.example/"), false).is_ok()
        );
        let addrs = |list: &[&str]| -> Vec<SocketAddr> {
            list.iter().map(|a| a.parse().unwrap()).collect()
        };
        assert_eq!(
            public_addrs("x", addrs(&["10.0.0.1:0", "93.184.216.34:0"]), false).unwrap(),
            addrs(&["93.184.216.34:0"])
        );
        assert!(public_addrs("x", addrs(&["169.254.169.254:0"]), false).is_err());
        assert_eq!(
            public_addrs("x", addrs(&["10.0.0.1:0"]), true)
                .unwrap()
                .len(),
            1
        );
        assert!(targets_private(&url("http://127.0.0.1:9/spec.md")));
        assert!(targets_private(&url("http://localhost/spec.md")));
        assert!(!targets_private(&url("http://93.184.216.34/spec.md")));
    }

    #[test]
    fn local_servers_may_redirect_within_private_addresses() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
            thread,
        };
        // A user-given loopback URL may redirect to another loopback port.
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        let first = TcpListener::bind("127.0.0.1:0").unwrap();
        let next = format!("http://{}/spec.md", target.local_addr().unwrap());
        let url = format!("http://{}/start", first.local_addr().unwrap());
        let serve = |listener: TcpListener, reply: String| {
            thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                let mut bytes = [0; 4096];
                let _ = socket.read(&mut bytes).unwrap();
                let _ = socket.write_all(reply.as_bytes());
            })
        };
        let a = serve(
            first,
            format!(
                "HTTP/1.1 302 Found\r\nLocation: {next}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
        );
        let b = serve(
            target,
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 6\r\nConnection: close\r\n\r\n# Task".into(),
        );
        assert_eq!(read_url(&url, &AtomicBool::new(false)).unwrap(), "# Task");
        a.join().unwrap();
        b.join().unwrap();
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
