//! Operator-supplied settings, and the defaults that apply when there are none.
//!
//! Every field is optional. A relay with no config file at all is a useful relay —
//! bound to loopback, refusing private hosts, capped — so that the first run of a
//! freshly cloned checkout is safe rather than merely convenient.

use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// 20 GiB. Large enough for any single file a browser can realistically write,
/// small enough that a runaway response cannot fill a small server's pipe forever.
const DEFAULT_MAX_BYTES: u64 = 21_474_836_480;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Loopback by default. Exposing the relay to a network is a decision the
    /// operator makes deliberately, not one they make by forgetting to set this.
    pub bind: String,
    /// CORS origins. `["*"]` is the useful default because a self-hosted relay
    /// usually has exactly one client — the operator — and no session to steal.
    pub allowed_origins: Vec<String>,
    /// When non-empty, the only upstream hosts the relay will talk to. Empty means
    /// "anything the compliance policy already permits", which is not the same as
    /// "anything".
    pub allow_hosts: Vec<String>,
    /// Refuse to relay a web document (an HTML/XHTML response) unless its host is in
    /// [`Config::fetch_document_hosts`]. This is what stops a public relay being used as a
    /// general web proxy: media, manifests and JSON still pass from anywhere (that is the
    /// relay's job and includes direct links), but "fetch me any web page" is refused.
    /// Off by default so a self-hosted relay behaves exactly as before.
    pub refuse_documents: bool,
    /// Hosts (and their subdomains) whose HTML the relay will still serve when
    /// [`Config::refuse_documents`] is on — the sites whose extraction genuinely reads a
    /// page rather than an API. Ignored when `refuse_documents` is off.
    pub fetch_document_hosts: Vec<String>,
    /// Opt-in to relaying into private address space. Off by default because a relay
    /// with this on is a hole punched through the operator's network perimeter: every
    /// client of the relay can reach every host the relay can reach.
    pub allow_private_hosts: bool,
    /// Ceiling on a single relayed response, enforced both from `content-length` and
    /// again while streaming, because a chunked response declares no length.
    pub max_bytes: u64,
    /// Simultaneous upstream requests. The relay is a pipe, not a cache, so this is
    /// the only thing standing between it and its own bandwidth bill.
    pub max_concurrent: usize,
    /// Connect plus response-headers timeout. Deliberately **not** a whole-body
    /// timeout: a 20 GiB file over a slow link is the normal case here, not an abuse.
    pub timeout_seconds: u64,
    /// Server-side YouTube extraction (the `/youtube` route). Off by default: it shells
    /// out to `yt-dlp`, which is a dependency and a maintenance commitment the operator
    /// opts into, not one they inherit by running the plain proxy.
    pub youtube: YoutubeConfig,
}

/// Settings for the `/youtube` route.
///
/// The plain proxy relays bytes a page already knows how to ask for. YouTube is different:
/// the media URLs are short-lived, `n`-throttled, and gated behind a Proof-of-Origin token
/// that only a real, trusted session mints — which is why the extension's direct fetch
/// dies at ~60s. `yt-dlp` does that whole dance, so the relay drives it and streams the
/// result. It is the one route that runs an external program.
///
/// **The trust caveat that decides whether this works at all:** YouTube flags datacenter
/// IPs hardest. On a residential IP (a laptop, a home server) `yt-dlp` downloads full
/// videos with none of this set. On a cloud VM it will hit the same ~60s wall the
/// extension did unless `cookies` and an egress `proxy` (residential) are supplied. This
/// is not a bug in the relay; it is the arms race, and these two fields are how you pay
/// into it.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct YoutubeConfig {
    /// Off by default. With this false the `/youtube` route answers 404, exactly as if it
    /// did not exist, so a relay that only proxies never spawns a subprocess.
    pub enabled: bool,
    /// The `yt-dlp` binary. A bare name is looked up on `PATH`; an absolute path pins it.
    pub ytdlp_path: String,
    /// A JavaScript runtime for `yt-dlp` (e.g. `deno`). Without one, current `yt-dlp`
    /// cannot decipher the `n` parameter for some clients and warns that formats may be
    /// missing — so a production install should set this. Empty leaves `yt-dlp` to its
    /// own default detection.
    pub js_runtime: String,
    /// Path to a Netscape-format cookies file. On a datacenter IP this is often what makes
    /// extraction work at all; on a residential IP it is usually unnecessary. Empty means
    /// none.
    pub cookies_file: String,
    /// An egress proxy for `yt-dlp` (`http://…`, `socks5://…`). A residential proxy is the
    /// other half of making this work from a cloud host. Empty means direct.
    pub proxy: String,
    /// The highest video height the route will serve. The free tier is 1080p; higher
    /// resolutions are the credit-gated path (enforced by the caller, capped here too so
    /// the relay is never the weak link). A request may ask for less but never more.
    pub max_height: u32,
}

impl Default for YoutubeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ytdlp_path: "yt-dlp".to_string(),
            js_runtime: String::new(),
            cookies_file: String::new(),
            proxy: String::new(),
            max_height: 1080,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8088".to_string(),
            allowed_origins: vec!["*".to_string()],
            allow_hosts: Vec::new(),
            refuse_documents: false,
            fetch_document_hosts: Vec::new(),
            allow_private_hosts: false,
            max_bytes: DEFAULT_MAX_BYTES,
            max_concurrent: 8,
            timeout_seconds: 30,
            youtube: YoutubeConfig::default(),
        }
    }
}

impl Config {
    /// Load from an explicit path, or from `dl-relay.toml` beside the binary.
    ///
    /// A path the operator named and that does not exist is an error; the implicit
    /// path simply not being there is not. Silently ignoring a typo'd `--config`
    /// would start a relay configured differently from the one that was asked for.
    pub fn load(explicit: Option<&str>) -> Result<Self, String> {
        match explicit {
            Some(path) => {
                let text = std::fs::read_to_string(path)
                    .map_err(|e| format!("cannot read config {path}: {e}"))?;
                toml::from_str(&text).map_err(|e| format!("cannot parse config {path}: {e}"))
            }
            None => match default_path() {
                Some(path) if path.exists() => {
                    let text = std::fs::read_to_string(&path)
                        .map_err(|e| format!("cannot read config {}: {e}", path.display()))?;
                    toml::from_str(&text)
                        .map_err(|e| format!("cannot parse config {}: {e}", path.display()))
                }
                _ => Ok(Self::default()),
            },
        }
    }

    /// True when `origin` may be answered by the CORS layer.
    pub fn allows_any_origin(&self) -> bool {
        self.allowed_origins.iter().any(|o| o == "*")
    }

    pub fn parse_bind(&self) -> Result<SocketAddr, String> {
        self.bind
            .parse()
            .map_err(|e| format!("bind is not a socket address ({}): {e}", self.bind))
    }
}

fn default_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(Path::new(exe.parent()?).join("dl-relay.toml"))
}

/// Printed verbatim at startup so an operator debugging a refusal can see the
/// settings that produced it rather than the ones they believe are in effect.
impl fmt::Display for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "  bind                = {}", self.bind)?;
        writeln!(f, "  allowed_origins     = {:?}", self.allowed_origins)?;
        writeln!(
            f,
            "  allow_hosts         = {}",
            if self.allow_hosts.is_empty() {
                "[] (any host the policy permits)".to_string()
            } else {
                format!("{:?}", self.allow_hosts)
            }
        )?;
        writeln!(f, "  refuse_documents    = {}", self.refuse_documents)?;
        if self.refuse_documents {
            writeln!(
                f,
                "  fetch_document_hosts= {}",
                if self.fetch_document_hosts.is_empty() {
                    "[] (no host may return HTML)".to_string()
                } else {
                    format!("{:?}", self.fetch_document_hosts)
                }
            )?;
        }
        writeln!(f, "  allow_private_hosts = {}", self.allow_private_hosts)?;
        writeln!(f, "  max_bytes           = {}", self.max_bytes)?;
        writeln!(f, "  max_concurrent      = {}", self.max_concurrent)?;
        writeln!(f, "  timeout_seconds     = {}", self.timeout_seconds)?;
        if self.youtube.enabled {
            writeln!(f, "  youtube.enabled     = true")?;
            writeln!(f, "  youtube.ytdlp_path  = {}", self.youtube.ytdlp_path)?;
            writeln!(
                f,
                "  youtube.js_runtime  = {}",
                if self.youtube.js_runtime.is_empty() {
                    "(yt-dlp default)"
                } else {
                    &self.youtube.js_runtime
                }
            )?;
            writeln!(
                f,
                "  youtube.cookies     = {}",
                if self.youtube.cookies_file.is_empty() {
                    "(none)"
                } else {
                    &self.youtube.cookies_file
                }
            )?;
            writeln!(
                f,
                "  youtube.proxy       = {}",
                if self.youtube.proxy.is_empty() {
                    "(direct)"
                } else {
                    &self.youtube.proxy
                }
            )?;
            write!(f, "  youtube.max_height  = {}", self.youtube.max_height)
        } else {
            write!(f, "  youtube.enabled     = false")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_config_file_yields_the_documented_defaults() {
        let cfg: Config = toml::from_str("").expect("empty toml is a valid config");
        assert_eq!(cfg.bind, "127.0.0.1:8088");
        assert_eq!(cfg.max_bytes, 21_474_836_480);
        assert_eq!(cfg.max_concurrent, 8);
        assert_eq!(cfg.timeout_seconds, 30);
        assert!(!cfg.allow_private_hosts);
        assert!(cfg.allow_hosts.is_empty());
        assert!(cfg.allows_any_origin());
    }

    #[test]
    fn a_partial_config_overrides_only_what_it_names() {
        let cfg: Config = toml::from_str("max_concurrent = 2\nallow_hosts = [\"cdn.example.com\"]")
            .expect("partial toml is a valid config");
        assert_eq!(cfg.max_concurrent, 2);
        assert_eq!(cfg.allow_hosts, vec!["cdn.example.com".to_string()]);
        assert_eq!(cfg.bind, "127.0.0.1:8088");
    }

    #[test]
    fn an_unknown_key_is_an_error_rather_than_a_silent_no_op() {
        let err = toml::from_str::<Config>("max_bites = 10").unwrap_err();
        assert!(err.to_string().contains("max_bites"), "{err}");
    }

    #[test]
    fn an_explicit_origin_list_is_not_a_wildcard() {
        let cfg: Config = toml::from_str("allowed_origins = [\"https://app.example.com\"]")
            .expect("valid config");
        assert!(!cfg.allows_any_origin());
    }

    #[test]
    fn document_refusal_is_off_by_default_and_configurable() {
        let cfg = Config::default();
        assert!(!cfg.refuse_documents);
        assert!(cfg.fetch_document_hosts.is_empty());

        let cfg: Config = toml::from_str(
            "refuse_documents = true\nfetch_document_hosts = [\"bilibili.com\", \"vimeo.com\"]",
        )
        .expect("valid config");
        assert!(cfg.refuse_documents);
        assert_eq!(cfg.fetch_document_hosts, vec!["bilibili.com", "vimeo.com"]);
    }

    #[test]
    fn youtube_is_off_and_capped_at_1080p_by_default() {
        let cfg = Config::default();
        assert!(!cfg.youtube.enabled);
        assert_eq!(cfg.youtube.max_height, 1080);
        assert_eq!(cfg.youtube.ytdlp_path, "yt-dlp");
        assert!(cfg.youtube.cookies_file.is_empty());
        assert!(cfg.youtube.proxy.is_empty());
    }

    #[test]
    fn a_youtube_section_overrides_only_what_it_names() {
        let cfg: Config = toml::from_str(
            "[youtube]\nenabled = true\nmax_height = 2160\ncookies_file = \"/etc/dl-relay/cookies.txt\"",
        )
        .expect("valid config");
        assert!(cfg.youtube.enabled);
        assert_eq!(cfg.youtube.max_height, 2160);
        assert_eq!(cfg.youtube.cookies_file, "/etc/dl-relay/cookies.txt");
        // Untouched fields keep their defaults.
        assert_eq!(cfg.youtube.ytdlp_path, "yt-dlp");
        assert!(cfg.youtube.proxy.is_empty());
    }

    #[test]
    fn an_unknown_key_inside_youtube_is_an_error() {
        let err = toml::from_str::<Config>("[youtube]\nenabled = true\ncookie = \"x\"").unwrap_err();
        assert!(err.to_string().contains("cookie"), "{err}");
    }
}
