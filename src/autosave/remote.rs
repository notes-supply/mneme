//! Where a service syncs: a Mneme server, or any Git remote by its URL. A server checks every
//! push and takes the service's reports. Any other remote only stores what it is sent, and the
//! service logs in to it with the user's own ssh, or with a token over HTTPS.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use super::Token;

/// Where a service syncs, and how it logs in.
#[derive(Clone, Debug)]
pub enum Remote {
    /// A Mneme server, by its base URL, such as `https://mneme.example`, with this machine's
    /// token. Its Git routes are below the URL.
    Server { url: String, token: Token },
    /// Any Git remote, by the URL `origin` has.
    Git { url: String, login: Login },
}

/// How the service logs in to a Git remote that is not a Mneme server.
#[derive(Clone, Debug, Default)]
pub struct Login {
    /// For SSH, a key that ssh uses alone, such as a deploy key of this machine's own.
    pub ssh_key: Option<PathBuf>,
    /// For SSH, whether ssh learns the key of a host it has never seen. Only `init` does, so
    /// that its first clone can, and the service itself never accepts a new key.
    pub accept_new_host_key: bool,
    /// For HTTPS, a token sent as the password of HTTP Basic authentication.
    pub token: Option<Token>,
}

/// How Git reaches a remote, by the rules Git itself reads a URL with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// `ssh://`, `git+ssh://` or `ssh+git://`, or the short form `[user@]host:path`.
    Ssh,
    /// `https://`, or `http://`.
    Http,
    /// A path, or `file://`.
    Local,
    /// Any other scheme, or a remote helper's `name::address`.
    Other,
}

impl Transport {
    /// How Git reaches `url`. A URL is `scheme://...`, a remote helper is `name::...`, and
    /// otherwise a colon before any slash makes it the short form of SSH, as in
    /// `git@example.com:wiki.git`.
    #[must_use]
    pub fn of(url: &str) -> Self {
        let scheme_end = url
            .char_indices()
            .find(|&(at, c)| {
                let scheme = if at == 0 {
                    c.is_ascii_alphabetic()
                } else {
                    c.is_ascii_alphanumeric() || "+-.".contains(c)
                };
                !scheme
            })
            .map_or(url.len(), |(at, _)| at);
        let rest = &url[scheme_end..];
        if scheme_end > 0 && rest.starts_with("://") {
            return match url[..scheme_end].to_ascii_lowercase().as_str() {
                "ssh" | "git+ssh" | "ssh+git" => Self::Ssh,
                "http" | "https" => Self::Http,
                "file" => Self::Local,
                _ => Self::Other,
            };
        }
        if scheme_end > 0 && rest.starts_with("::") {
            return Self::Other;
        }
        match (url.find(':'), url.find('/')) {
            (None, _) => Self::Local,
            (Some(colon), Some(slash)) if slash < colon => Self::Local,
            (Some(_), _) => Self::Ssh,
        }
    }
}

impl Remote {
    /// A Git remote by `url`, checked against how it logs in. A local path is made absolute,
    /// since Git runs in the folder and a path relative to elsewhere would not be found there.
    ///
    /// # Errors
    ///
    /// Fails when the login does not fit the remote: a key for anything but SSH, a token for
    /// anything but HTTPS, or a password in an HTTPS URL, which belongs in a token file. A token
    /// over plain `http://` is refused too unless the host is this machine, since it would cross
    /// the network in clear text, and so is a key whose path is not UTF-8, since ssh gets it in
    /// a line of configuration.
    pub fn git(url: &str, login: Login) -> Result<Self> {
        let transport = Transport::of(url);
        if let Some(key) = &login.ssh_key {
            if transport != Transport::Ssh {
                bail!("--ssh-key is for an SSH remote, and {url} is not one");
            }
            if key.to_str().is_none() {
                bail!("the key's path {} is not UTF-8", key.display());
            }
        }
        if login.token.is_some() && transport != Transport::Http {
            bail!("--token-file is for an HTTPS remote, and {url} is not one");
        }
        if login.token.is_some() && in_clear(url) {
            bail!(
                "{url} is plain http, which would send the token in clear text. Use an \
                 https:// URL, or http:// only to localhost, 127.0.0.1 or [::1]"
            );
        }
        if transport == Transport::Http && userinfo(url).is_some_and(|info| info.contains(':')) {
            bail!(
                "the remote's URL holds a password. Put the token in a file and pass \
                 --token-file instead, so that it stays out of the URL and the logs"
            );
        }
        let url = if transport == Transport::Local && !url.starts_with("file:") {
            let absolute = std::path::absolute(Path::new(url))
                .with_context(|| format!("finding the remote {url}"))?;
            absolute
                .to_str()
                .with_context(|| format!("the remote's path {} is not UTF-8", absolute.display()))?
                .to_owned()
        } else {
            url.to_owned()
        };
        Ok(Self::Git { url, login })
    }

    /// The URL `origin` names.
    #[must_use]
    pub fn origin(&self) -> String {
        match self {
            Self::Server { url, .. } => format!("{url}/git/wiki.git"),
            Self::Git { url, .. } => url.clone(),
        }
    }

    /// Whether the remote is a Mneme server, which takes reports and checks every push.
    #[must_use]
    pub fn is_server(&self) -> bool {
        matches!(self, Self::Server { .. })
    }

    /// The configuration that logs in to the remote, which every Git command gets through
    /// `GIT_CONFIG_*`, never on a command line. A header is scoped to the remote's URL, so Git
    /// sends it nowhere else.
    #[must_use]
    pub fn login_settings(&self) -> Vec<(String, String)> {
        match self {
            Self::Server { url, token } => vec![(
                format!("http.{url}.extraHeader"),
                format!("Authorization: Bearer {}", token.0),
            )],
            Self::Git { url, login } => match Transport::of(url) {
                Transport::Ssh => vec![("core.sshCommand".to_owned(), ssh_command(login))],
                Transport::Http => login
                    .token
                    .iter()
                    .map(|token| {
                        (
                            format!("http.{url}.extraHeader"),
                            format!("Authorization: Basic {}", basic(url, &token.0)),
                        )
                    })
                    .collect(),
                Transport::Local | Transport::Other => Vec::new(),
            },
        }
    }
}

/// The user's own ssh, which never asks a person anything. ssh still reads `~/.ssh/config`,
/// the user's keys and the agent, so a host alias, a jump host or Tailscale SSH works as it does
/// in a terminal.
fn ssh_command(login: &Login) -> String {
    let mut command = "ssh -o BatchMode=yes".to_owned();
    if let Some(key) = &login.ssh_key {
        command.push_str(" -i ");
        command.push_str(&shell_quote(&key.to_string_lossy()));
        command.push_str(" -o IdentitiesOnly=yes");
    }
    if login.accept_new_host_key {
        command.push_str(" -o StrictHostKeyChecking=accept-new");
    }
    command
}

/// `text` as one word for the shell that Git runs `core.sshCommand` with.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The credentials of HTTP Basic authentication: the URL's user name, or `git` when it has none,
/// and the token as the password.
fn basic(url: &str, token: &str) -> String {
    let user = userinfo(url).map_or_else(|| "git".to_owned(), percent_decode);
    STANDARD.encode(format!("{user}:{token}"))
}

/// What comes before the `@` of a URL's authority, such as `alice` in
/// `https://alice@example.com/wiki.git`.
fn userinfo(url: &str) -> Option<&str> {
    let (_, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    authority.rsplit_once('@').map(|(info, _)| info)
}

/// Whether `url` is plain `http://` to a host other than this machine, so that whatever it is
/// sent crosses the network unencrypted.
fn in_clear(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("http") {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_and_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = if host_and_port.starts_with('[') {
        host_and_port
            .find(']')
            .map_or(host_and_port, |end| &host_and_port[..=end])
    } else {
        host_and_port.split(':').next().unwrap_or_default()
    };
    !(host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "[::1]")
}

/// Decodes `%XX` escapes, as Git does for a URL's user name. A broken escape stays as it is.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let escaped = (bytes[at] == b'%')
            .then(|| bytes.get(at + 1..at + 3))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        if let Some(byte) = escaped {
            out.push(byte);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_is_read_as_git_reads_it() {
        for (url, transport) in [
            ("ssh://host/~/wiki.git", Transport::Ssh),
            ("ssh://git@host:2222/srv/wiki.git", Transport::Ssh),
            ("git+ssh://host/wiki.git", Transport::Ssh),
            ("SSH://host/wiki.git", Transport::Ssh),
            ("git@github.com:someone/wiki.git", Transport::Ssh),
            ("host:wiki.git", Transport::Ssh),
            ("https://example.com/wiki.git", Transport::Http),
            ("http://127.0.0.1:8080/wiki.git", Transport::Http),
            ("file:///srv/wiki.git", Transport::Local),
            ("/srv/wiki.git", Transport::Local),
            ("wiki.git", Transport::Local),
            ("./a:b/wiki.git", Transport::Local),
            ("git://example.com/wiki.git", Transport::Other),
            ("ext::ssh -i key host %S wiki.git", Transport::Other),
        ] {
            assert_eq!(Transport::of(url), transport, "{url}");
        }
    }

    fn login(remote: &Remote) -> Vec<(String, String)> {
        remote.login_settings()
    }

    #[test]
    fn ssh_never_prompts_and_learns_a_host_key_only_when_asked() {
        let plain = Remote::git("ssh://host/~/wiki.git", Login::default()).unwrap();
        assert_eq!(
            login(&plain),
            [("core.sshCommand".into(), "ssh -o BatchMode=yes".into())]
        );
        let keyed = Remote::git(
            "git@example.com:wiki.git",
            Login {
                ssh_key: Some("/home/u/.ssh/wiki deploy's key".into()),
                accept_new_host_key: true,
                token: None,
            },
        )
        .unwrap();
        assert_eq!(
            login(&keyed),
            [(
                "core.sshCommand".into(),
                r"ssh -o BatchMode=yes -i '/home/u/.ssh/wiki deploy'\''s key' -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new".into()
            )]
        );
    }

    #[test]
    fn an_https_token_is_basic_authentication_scoped_to_the_url() {
        let token = || Login {
            token: Some(Token("s3cret".to_owned())),
            ..Login::default()
        };
        let url = "https://example.com/wiki.git";
        let anonymous = Remote::git(url, token()).unwrap();
        assert_eq!(
            login(&anonymous),
            [(
                format!("http.{url}.extraHeader"),
                format!("Authorization: Basic {}", STANDARD.encode("git:s3cret")),
            )]
        );
        let url = "https://x%40y@example.com:8443/wiki.git";
        let named = Remote::git(url, token()).unwrap();
        assert_eq!(
            login(&named),
            [(
                format!("http.{url}.extraHeader"),
                format!("Authorization: Basic {}", STANDARD.encode("x@y:s3cret")),
            )]
        );
        let without = Remote::git("https://example.com/wiki.git", Login::default()).unwrap();
        assert!(login(&without).is_empty(), "no token, no header");
    }

    #[test]
    fn a_login_that_does_not_fit_the_remote_is_refused() {
        let key = || Login {
            ssh_key: Some("/key".into()),
            ..Login::default()
        };
        let token = || Login {
            token: Some(Token("t".to_owned())),
            ..Login::default()
        };
        for (url, login) in [
            ("https://example.com/wiki.git", key()),
            ("/srv/wiki.git", key()),
            ("ssh://host/wiki.git", token()),
            ("file:///srv/wiki.git", token()),
            (
                "https://user:password@example.com/wiki.git",
                Login::default(),
            ),
        ] {
            assert!(Remote::git(url, login).is_err(), "{url}");
        }
    }

    #[test]
    fn a_token_over_plain_http_is_refused_unless_the_host_is_this_machine() {
        let token = || Login {
            token: Some(Token("s3cret".to_owned())),
            ..Login::default()
        };
        for url in [
            "http://example.com/wiki.git",
            "HTTP://example.com:8080/wiki.git",
            "http://alice@example.com/wiki.git",
            "http://192.168.1.10/wiki.git",
            "http://127.0.0.2/wiki.git",
            "http://localhost.example.com/wiki.git",
            "http://[::2]:8080/wiki.git",
        ] {
            let refused = Remote::git(url, token()).unwrap_err().to_string();
            assert!(refused.contains("https"), "{url}: {refused}");
            assert!(!refused.contains("s3cret"), "{url}: {refused}");
        }
        for url in [
            "http://localhost/wiki.git",
            "http://LocalHost:8080/wiki.git",
            "http://127.0.0.1:8080/wiki.git",
            "http://alice@127.0.0.1:8080/wiki.git",
            "http://[::1]:8080/wiki.git",
            "https://example.com/wiki.git",
        ] {
            assert!(Remote::git(url, token()).is_ok(), "{url}");
        }
        assert!(
            Remote::git("http://example.com/wiki.git", Login::default()).is_ok(),
            "without a token, nothing secret is sent"
        );
    }

    #[test]
    fn a_local_remote_is_made_absolute_and_a_url_stays_as_given() {
        let Remote::Git { url, .. } = Remote::git("wiki.git", Login::default()).unwrap() else {
            panic!("a Git remote");
        };
        assert!(Path::new(&url).is_absolute(), "{url}");
        assert!(url.ends_with("/wiki.git"), "{url}");
        for given in [
            "file:///srv/wiki.git",
            "ssh://host/~/wiki.git",
            "host:wiki.git",
        ] {
            let remote = Remote::git(given, Login::default()).unwrap();
            assert_eq!(remote.origin(), given);
        }
    }

    #[test]
    fn a_token_never_shows_in_debug_output() {
        let remote = Remote::git(
            "https://example.com/wiki.git",
            Login {
                token: Some(Token("s3cret".to_owned())),
                ..Login::default()
            },
        )
        .unwrap();
        assert!(!format!("{remote:?}").contains("s3cret"));
    }
}
