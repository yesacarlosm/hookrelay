//! Per-source verification settings, loaded from the environment at startup.

use std::{collections::HashMap, fmt, time::Duration};

use crate::verify::{DEFAULT_STRIPE_TOLERANCE, Scheme, UnknownScheme};

pub const SECRET_PREFIX: &str = "HOOKRELAY_SECRET_";
pub const SCHEME_PREFIX: &str = "HOOKRELAY_SCHEME_";

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
    pub stripe_tolerance: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sources: HashMap::new(),
            stripe_tolerance: DEFAULT_STRIPE_TOLERANCE,
        }
    }
}

impl Config {
    /// Builds the config from `HOOKRELAY_SECRET_<SOURCE>` and `HOOKRELAY_SCHEME_<SOURCE>`
    /// variables. Other variables are ignored.
    ///
    /// The scheme defaults to the source name for `github` and `stripe`; any other source
    /// with a secret must set its scheme explicitly, so verification is never guessed.
    pub fn from_vars<I>(vars: I, stripe_tolerance: Duration) -> Result<Self, ConfigError>
    where
        I: IntoIterator<Item = (String, String)>,
    {
        let mut secrets = HashMap::new();
        let mut schemes = HashMap::new();
        for (var, value) in vars {
            if let Some(suffix) = var.strip_prefix(SECRET_PREFIX) {
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
}

fn normalize(source: &str) -> String {
    source.replace('-', "_")
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
    fn secret_debug_is_redacted() {
        let config = Config::default().with_source("github", Scheme::Github, "hunter2");
        assert!(!format!("{config:?}").contains("hunter2"));
    }
}
