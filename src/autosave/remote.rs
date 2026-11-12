//! Where a service syncs: a Mneme server, or a Git remote by its URL. A server checks every
//! push and takes the service's reports. A Git remote, a Git host over HTTPS or a repository
//! on this machine, only stores what it is sent, and the service logs in to a Git host with a
//! token.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use super::Token;

/// The refusal of a remote Mneme does not sync with, which says what it syncs with.
const UNSUPPORTED: &str = "--remote takes a Git host over HTTPS (https://HOST/PATH, with \
     --token-file for its token) or a repository on this machine (a path or a file:// URL), \
     and a Mneme server is --server URL. Plain http:// works only to localhost, 127.0.0.1 or \
     [::1], and SSH, git:// and remote helpers are not supported";

/// Where a service syncs, and how it logs in.
#[derive(Clone, Debug)]
pub enum Remote {
    /// A Mneme server, by its base URL, such as `https://mneme.example`, with this machine's
    /// token. Its Git routes are below the URL.
    Server { url: String, token: Token },
    /// A Git host over HTTPS or a repository on this machine, by the URL `origin` has.
    Git { url: String, login: Login },
}

/// How the service logs in to a Git remote that is not a Mneme server.
#[derive(Clone, Debug, Default)]
pub struct Login {
    /// For HTTPS, a token sent as the password of HTTP Basic authentication.
    pub token: Option<Token>,
}

/// How Git reaches a remote that Mneme syncs with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// `https://`, or `http://` to this machine.
    Http,
    /// A path, or `file://`.
    Local,
}

impl Transport {
    /// How Git reaches `url`, or `None` for a remote Mneme does not sync with. Git reads
    /// `scheme://...` as a URL and `name::...` as a remote helper, and otherwise a colon before
    /// any slash as the short form of SSH, as in `git@example.com:wiki.git`, and the rest as a
    /// path. Of these, only `https://`, `http://` to localhost, `127.0.0.1` or `[::1]`,
    /// `file://` and a path are taken.
    #[must_use]
    pub fn of(url: &str) -> Option<Self> {
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
                "https" => Some(Self::Http),
                "http" if !in_clear(url) => Some(Self::Http),
                "file" => Some(Self::Local),
                _ => None,
            };
        }
        if scheme_end > 0 && rest.starts_with("::") {
            return None;
        }
        match (url.find(':'), url.find('/')) {
            (None, _) => Some(Self::Local),
            (Some(colon), Some(slash)) if slash < colon => Some(Self::Local),
            (Some(_), _) => None,
        }
    }
}

impl Remote {
    /// A Git remote by `url`, checked against how it logs in. A local path is made absolute,
    /// since Git runs in the folder and a path relative to elsewhere would not be found there.
    /// A URL's scheme is made lower case, since Git looks for a remote helper named as written,
    /// such as `git-remote-HTTPS` for `HTTPS://`.
    ///
    /// # Errors
    ///
    /// Fails for a remote Mneme does not sync with, such as SSH, `git://`, a remote helper or
    /// plain `http://` to another machine, which would send the wiki and any token in clear
    /// text. Fails too when the login does not fit the remote: a token for anything but HTTPS,
    /// or a password in an HTTPS URL, which belongs in a token file.
    pub fn git(url: &str, login: Login) -> Result<Self> {
        let Some(transport) = Transport::of(url) else {
            bail!(UNSUPPORTED);
        };
        if login.token.is_some() && transport != Transport::Http {
            bail!("--token-file is for an HTTPS remote, and {url} is not one");
        }
        if transport == Transport::Http && userinfo(url).is_some_and(|info| info.contains(':')) {
            bail!(
                "the remote's URL holds a password. Put the token in a file and pass \
                 --token-file instead, so that it stays out of the URL and the logs"
            );
        }
        let url = if transport == Transport::Local && file_url(url).is_none() {
            let absolute = std::path::absolute(Path::new(url))
                .with_context(|| format!("finding the remote {url}"))?;
            absolute
                .to_str()
                .with_context(|| format!("the remote's path {} is not UTF-8", absolute.display()))?
                .to_owned()
        } else {
            match url.split_once("://") {
                Some((scheme, rest)) => format!("{}://{rest}", scheme.to_ascii_lowercase()),
                None => url.to_owned(),
            }
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

    /// Where the repository is on this machine, for a remote that is a path or a `file://`
    /// URL. A URL is read as Git reads it: its `%XX` escapes are decoded, and its host may be
    /// `localhost` or left out.
    ///
    /// # Errors
    ///
    /// Fails for a `file://` URL that names another host, since its path is not on this
    /// machine.
    pub fn local_path(&self) -> Result<Option<PathBuf>> {
        let Self::Git { url, .. } = self else {
            return Ok(None);
        };
        if Transport::of(url) != Some(Transport::Local) {
            return Ok(None);
        }
        let Some(rest) = file_url(url) else {
            return Ok(Some(PathBuf::from(url)));
        };
        let path = match rest.find('/') {
            Some(0) => rest,
            Some(at) if rest[..at].eq_ignore_ascii_case("localhost") => &rest[at..],
            _ => bail!(
                "{url} names a host other than localhost, so it is not a path on this machine"
            ),
        };
        Ok(Some(PathBuf::from(percent_decode(path))))
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
            Self::Git { url, login } => login
                .token
                .iter()
                .map(|token| {
                    (
                        format!("http.{url}.extraHeader"),
                        format!("Authorization: Basic {}", basic(url, &token.0)),
                    )
                })
                .collect(),
        }
    }
}

/// The credentials of HTTP Basic authentication: the URL's user name, or `git` when it has none,
/// and the token as the password.
fn basic(url: &str, token: &str) -> String {
    let user = userinfo(url).map_or_else(|| "git".to_owned(), percent_decode);
    STANDARD.encode(format!("{user}:{token}"))
}

/// What follows `file://` in a `file:` URL, in any case.
fn file_url(url: &str) -> Option<&str> {
    url.get(..7)
        .filter(|scheme| scheme.eq_ignore_ascii_case("file://"))
        .map(|_| &url[7..])
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

    fn token() -> Login {
        Login {
            token: Some(Token("s3cret".to_owned())),
        }
    }

    #[test]
    fn a_url_is_read_as_git_reads_it() {
        for (url, transport) in [
            ("https://example.com/wiki.git", Transport::Http),
            ("HTTPS://example.com/wiki.git", Transport::Http),
            ("http://127.0.0.1:8080/wiki.git", Transport::Http),
            ("file:///srv/wiki.git", Transport::Local),
            ("/srv/wiki.git", Transport::Local),
            ("wiki.git", Transport::Local),
            ("./a:b/wiki.git", Transport::Local),
        ] {
            assert_eq!(Transport::of(url), Some(transport), "{url}");
        }
    }

    #[test]
    fn a_remote_mneme_does_not_sync_with_is_refused_with_what_it_takes() {
        for url in [
            "ssh://host/~/wiki.git",
            "ssh://git@host:2222/srv/wiki.git",
            "SSH://host/wiki.git",
            "git+ssh://host/wiki.git",
            "ssh+git://host/wiki.git",
            "git@github.com:someone/wiki.git",
            "host:wiki.git",
            "git://example.com/wiki.git",
            "ext::ssh -i key host %S wiki.git",
            "fd::3",
            "https::example.com/wiki.git",
            "ftp://example.com/wiki.git",
            "http://example.com/wiki.git",
            "http://192.168.1.10/wiki.git",
        ] {
            assert_eq!(Transport::of(url), None, "{url}");
            for login in [Login::default(), token()] {
                let refused = Remote::git(url, login).unwrap_err().to_string();
                for named in ["--server", "https://", "--token-file", "a path", "SSH"] {
                    assert!(refused.contains(named), "{url}: {refused}");
                }
                assert!(!refused.contains("s3cret"), "{url}: {refused}");
            }
        }
    }

    fn login(remote: &Remote) -> Vec<(String, String)> {
        remote.login_settings()
    }

    #[test]
    fn an_https_token_is_basic_authentication_scoped_to_the_url() {
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
        let path = Remote::git("/srv/wiki.git", Login::default()).unwrap();
        assert!(login(&path).is_empty(), "a path needs no login");
    }

    #[test]
    fn a_login_that_does_not_fit_the_remote_is_refused() {
        for (url, login) in [
            ("/srv/wiki.git", token()),
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
            "https://example.com/wiki.git",
            "http://localhost:8080/wiki.git",
        ] {
            let remote = Remote::git(given, Login::default()).unwrap();
            assert_eq!(remote.origin(), given);
        }
    }

    #[test]
    fn a_urls_scheme_is_made_lower_case_and_the_rest_stays_as_given() {
        for (given, origin) in [
            (
                "HTTPS://Example.com/Team/Wiki.git",
                "https://Example.com/Team/Wiki.git",
            ),
            (
                "Http://LocalHost:8080/wiki.git",
                "http://LocalHost:8080/wiki.git",
            ),
            ("FILE:///srv/Wiki.git", "file:///srv/Wiki.git"),
        ] {
            let remote = Remote::git(given, Login::default()).unwrap();
            assert_eq!(remote.origin(), origin, "{given}");
        }
        let remote = Remote::git("HTTPS://example.com/wiki.git", token()).unwrap();
        assert_eq!(
            login(&remote)[0].0,
            "http.https://example.com/wiki.git.extraHeader",
            "the token is scoped to the URL Git is given"
        );
    }

    #[test]
    fn a_local_remote_has_its_path_on_this_machine() {
        let path = |url: &str| Remote::git(url, Login::default()).unwrap().local_path();
        assert_eq!(path("/srv/wiki.git").unwrap(), Some("/srv/wiki.git".into()));
        assert_eq!(
            path("file:///srv/a%20b.git").unwrap(),
            Some("/srv/a b.git".into())
        );
        assert_eq!(
            path("file://localhost/srv/wiki.git").unwrap(),
            Some("/srv/wiki.git".into())
        );
        assert_eq!(
            path("FILE://LocalHost/srv/wiki.git").unwrap(),
            Some("/srv/wiki.git".into())
        );
        assert!(path("file://example.com/srv/wiki.git").is_err());
        assert_eq!(path("https://example.com/wiki.git").unwrap(), None);
        let server = Remote::Server {
            url: "https://mneme.example".to_owned(),
            token: Token("t".to_owned()),
        };
        assert_eq!(server.local_path().unwrap(), None);
    }

    #[test]
    fn a_token_never_shows_in_debug_output() {
        let remote = Remote::git("https://example.com/wiki.git", token()).unwrap();
        assert!(!format!("{remote:?}").contains("s3cret"));
    }
}
