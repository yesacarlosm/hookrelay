//! Per-source verification and forwarding settings, loaded from the environment at startup.

use std::{collections::HashMap, fmt, time::Duration};

use reqwest::Url;

use crate::verify::{DEFAULT_STRIPE_TOLERANCE, Scheme, UnknownScheme};

pub const SECRET_PREFIX: &str = "HOOKRELAY_SECRET_";
pub const SCHEME_PREFIX: &str = "HOOKRELAY_SCHEME_";
pub const TARGET_PREFIX: &str = "HOOKRELAY_TARGET_";
pub const ADMIN_TOKEN_VAR: &str = "HOOKRELAY_ADMIN_TOKEN";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("{var}: `{suffix}` is not a valid source name")]
    InvalidSource { var: String, suffix: String },
    #[error("{var} is empty")]
    EmptySecret { var: String },
    #[error("{var}: {source}")]
    UnknownScheme { var: String, source: UnknownScheme },
    #[error("no signature scheme for source `{0}`: set {SCHEME_PREFIX}{upper}", upper = .0.to_ascii_uppercase())]
    MissingScheme(String),
    #[error("{SCHEME_PREFIX}{upper} is set but {SECRET_PREFIX}{upper} is not", upper = .0.to_ascii_uppercase())]
    SchemeWithoutSecret(String),
    // The URL itself is left out: it may carry a token.
    #[error("{var}: {reason}")]
    InvalidTarget { var: String, reason: String },
}

/// A signing secret. `Debug` is redacted so it can't end up in logs by accident.
#[derive(Clone)]
pub struct Secret(Vec<u8>);

impl Secret {
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

#[derive(Debug, Clone)]
pub struct SourceConfig {
    pub scheme: Scheme,
    pub secret: Secret,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Keyed by source name with `-` normalized to `_`, since env var names can't contain `-`.
    sources: HashMap<String, SourceConfig>,
    /// Where to forward each source's hooks, keyed like `sources`.
    targets: HashMap<String, Url>,
    /// Bearer token for the admin endpoints; `None` leaves them open.
    admin_token: Option<Secret>,
    pub stripe_tolerance: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sources: HashMap::new(),
            targets: HashMap::new(),
            admin_token: None,
            stripe_tolerance: DEFAULT_STRIPE_TOLERANCE,
        }
    }
}

impl Config {
    /// Builds the config from `HOOKRELAY_SECRET_<SOURCE>`, `HOOKRELAY_SCHEME_<SOURCE>`,
    /// `HOOKRELAY_TARGET_<SOURCE>` and `HOOKRELAY_ADMIN_TOKEN`. Other variables are ignored.
    ///
    /// The scheme defaults to the source name for `github` and `stripe`; any other source
    /// with a secret must set its scheme explicitly, so verification is never guessed.
    pub fn from_vars<I>(vars: I, stripe_tolerance: Duration) -> Result<Self, ConfigError>
    where
        I: IntoIterator<Item = (String, String)>,
    {
        let mut secrets = HashMap::new();
        let mut schemes = HashMap::new();
        let mut targets = HashMap::new();
        let mut admin_token = None;
        for (var, value) in vars {
            if var == ADMIN_TOKEN_VAR {
                if value.is_empty() {
                    return Err(ConfigError::EmptySecret { var });
                }
                admin_token = Some(Secret(value.into_bytes()));
            } else if let Some(suffix) = var.strip_prefix(SECRET_PREFIX) {
                let source = source_key(&var, suffix)?;
                if value.is_empty() {
                    return Err(ConfigError::EmptySecret { var });
                }
                secrets.insert(source, Secret(value.into_bytes()));
            } else if let Some(suffix) = var.strip_prefix(SCHEME_PREFIX) {
                let source = source_key(&var, suffix)?;
                let scheme = value
                    .parse()
                    .map_err(|source| ConfigError::UnknownScheme { var, source })?;
                schemes.insert(source, scheme);
            } else if let Some(suffix) = var.strip_prefix(TARGET_PREFIX) {
                let source = source_key(&var, suffix)?;
                let target = parse_target(&value)
                    .map_err(|reason| ConfigError::InvalidTarget { var, reason })?;
                targets.insert(source, target);
            }
        }

        if let Some(source) = schemes.keys().find(|s| !secrets.contains_key(*s)) {
            return Err(ConfigError::SchemeWithoutSecret(source.clone()));
        }

        let sources = secrets
            .into_iter()
            .map(|(source, secret)| {
                let scheme = match schemes.get(&source) {
                    Some(scheme) => *scheme,
                    None => source
                        .parse()
                        .map_err(|_| ConfigError::MissingScheme(source.clone()))?,
                };
                Ok((source, SourceConfig { scheme, secret }))
            })
            .collect::<Result<_, _>>()?;

        Ok(Self {
            sources,
            targets,
            admin_token,
            stripe_tolerance,
        })
    }

    /// Adds or replaces the settings for one source.
    pub fn with_source(mut self, source: &str, scheme: Scheme, secret: impl Into<Vec<u8>>) -> Self {
        let secret = Secret(secret.into());
        self.sources
            .insert(normalize(source), SourceConfig { scheme, secret });
        self
    }

    /// Requires `token` on the admin endpoints.
    pub fn with_admin_token(mut self, token: impl Into<Vec<u8>>) -> Self {
        self.admin_token = Some(Secret(token.into()));
        self
    }

    /// The admin endpoints' bearer token, or `None` if they are open.
    pub fn admin_token(&self) -> Option<&Secret> {
        self.admin_token.as_ref()
    }

    /// Forwards hooks from `source` to `target`, replacing any previous target.
    pub fn with_target(mut self, source: &str, target: Url) -> Self {
        self.targets.insert(normalize(source), target);
        self
    }

    /// The verification settings for `source`, or `None` if it isn't verified.
    pub fn source(&self, source: &str) -> Option<&SourceConfig> {
        self.sources.get(&normalize(source))
    }

    /// Verified sources and their schemes, sorted by name. Never includes secrets.
    pub fn verified_sources(&self) -> Vec<(&str, Scheme)> {
        let mut sources: Vec<_> = self
            .sources
            .iter()
            .map(|(name, source)| (name.as_str(), source.scheme))
            .collect();
        sources.sort_unstable_by_key(|(name, _)| *name);
        sources
    }

    /// Where hooks from `source` are forwarded, or `None` if they are only stored.
    pub fn target(&self, source: &str) -> Option<&Url> {
        self.targets.get(&normalize(source))
    }

    /// Forwarded sources and their targets, sorted by name.
    pub fn targets(&self) -> Vec<(&str, &Url)> {
        let mut targets: Vec<_> = self
            .targets
            .iter()
            .map(|(name, url)| (name.as_str(), url))
            .collect();
        targets.sort_unstable_by_key(|(name, _)| *name);
        targets
    }
}

fn parse_target(value: &str) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|err| err.to_string())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "scheme must be http or https, not `{}`",
            url.scheme()
        ));
    }
    Ok(url)
}

/// The single canonical key used for both storage and lookup.
fn normalize(source: &str) -> String {
    source.to_ascii_lowercase().replace('-', "_")
}

fn source_key(var: &str, suffix: &str) -> Result<String, ConfigError> {
    let key = suffix.to_ascii_lowercase();
    let valid = (1..=64).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if valid {
        Ok(key)
    } else {
        Err(ConfigError::InvalidSource {
            var: var.to_string(),
            suffix: suffix.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(vars: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let vars = vars.iter().map(|(k, v)| (k.to_string(), v.to_string()));
        Config::from_vars(vars, DEFAULT_STRIPE_TOLERANCE)
    }

    #[test]
    fn defaults_scheme_from_source_name() {
        let config = load(&[
            ("HOOKRELAY_SECRET_GITHUB", "gh"),
            ("HOOKRELAY_SECRET_STRIPE", "whsec_x"),
        ])
        .unwrap();
        assert_eq!(
            config.verified_sources(),
            [("github", Scheme::Github), ("stripe", Scheme::Stripe)]
        );
    }

    #[test]
    fn scheme_override_applies_to_any_source() {
        let config = load(&[
            ("HOOKRELAY_SECRET_BILLING", "whsec_x"),
            ("HOOKRELAY_SCHEME_BILLING", "stripe"),
        ])
        .unwrap();
        assert_eq!(config.source("billing").unwrap().scheme, Scheme::Stripe);
    }

    #[test]
    fn source_lookup_treats_dash_as_underscore() {
        let config = load(&[
            ("HOOKRELAY_SECRET_MY_APP", "s"),
            ("HOOKRELAY_SCHEME_MY_APP", "github"),
        ])
        .unwrap();
        assert!(config.source("my-app").is_some());
    }

    #[test]
    fn with_source_uses_the_same_canonical_key_as_lookup() {
        let config = Config::default().with_source("My-App", Scheme::Github, "s");
        assert!(config.source("my_app").is_some());
    }

    #[test]
    fn source_without_secret_is_unverified() {
        let config = load(&[("HOOKRELAY_SECRET_GITHUB", "gh")]).unwrap();
        assert!(config.source("stripe").is_none());
    }

    #[test]
    fn ignores_unrelated_vars() {
        let config = load(&[("HOOKRELAY_ADDR", "0.0.0.0:80"), ("PATH", "/bin")]).unwrap();
        assert!(config.verified_sources().is_empty());
    }

    #[test]
    fn rejects_unknown_source_without_scheme() {
        let err = load(&[("HOOKRELAY_SECRET_GITLAB", "s")]).unwrap_err();
        assert_eq!(err, ConfigError::MissingScheme("gitlab".into()));
    }

    #[test]
    fn rejects_scheme_without_secret() {
        let err = load(&[("HOOKRELAY_SCHEME_GITHUB", "github")]).unwrap_err();
        assert_eq!(err, ConfigError::SchemeWithoutSecret("github".into()));
    }

    #[test]
    fn rejects_empty_secret() {
        let err = load(&[("HOOKRELAY_SECRET_GITHUB", "")]).unwrap_err();
        assert!(matches!(err, ConfigError::EmptySecret { .. }));
    }

    #[test]
    fn rejects_unknown_scheme() {
        let err = load(&[
            ("HOOKRELAY_SECRET_GITHUB", "gh"),
            ("HOOKRELAY_SCHEME_GITHUB", "md5"),
        ])
        .unwrap_err();
        assert!(matches!(err, ConfigError::UnknownScheme { .. }));
    }

    #[test]
    fn rejects_invalid_source_name() {
        let err = load(&[("HOOKRELAY_SECRET_A.B", "s")]).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidSource { .. }));
    }

    #[test]
    fn loads_targets_for_any_source() {
        let config = load(&[
            ("HOOKRELAY_TARGET_GITHUB", "http://localhost:3000/github"),
            ("HOOKRELAY_TARGET_MY_APP", "https://example.com/in"),
        ])
        .unwrap();
        assert_eq!(
            config.target("github").unwrap().as_str(),
            "http://localhost:3000/github"
        );
        assert!(config.target("my-app").is_some());
        assert!(config.target("stripe").is_none());
        // A target doesn't turn on verification.
        assert!(config.verified_sources().is_empty());
    }

    #[test]
    fn rejects_invalid_target() {
        for url in ["not a url", "ftp://example.com/", "file:///etc/passwd"] {
            let err = load(&[("HOOKRELAY_TARGET_GITHUB", url)]).unwrap_err();
            assert!(
                matches!(err, ConfigError::InvalidTarget { .. }),
                "{url}: {err:?}"
            );
        }
    }

    #[test]
    fn loads_admin_token() {
        let config = load(&[("HOOKRELAY_ADMIN_TOKEN", "t0ken")]).unwrap();
        assert_eq!(config.admin_token().unwrap().expose(), b"t0ken");
        assert!(load(&[]).unwrap().admin_token().is_none());

        let err = load(&[("HOOKRELAY_ADMIN_TOKEN", "")]).unwrap_err();
        assert!(matches!(err, ConfigError::EmptySecret { .. }));
    }

    #[test]
    fn secret_debug_is_redacted() {
        let config = Config::default()
            .with_source("github", Scheme::Github, "hunter2")
            .with_admin_token("t0ken");
        let debug = format!("{config:?}");
        assert!(
            !debug.contains("hunter2") && !debug.contains("t0ken"),
            "{debug}"
        );
    }
}
