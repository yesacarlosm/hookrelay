//! Webhook signature verification.
//!
//! Pure functions over the raw header value and body, so they can be tested without a server.

use std::{str::FromStr, time::Duration};

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Stripe's recommended tolerance between the signed timestamp and the current time.
pub const DEFAULT_STRIPE_TOLERANCE: Duration = Duration::from_secs(300);

/// How a provider signs its deliveries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    /// `X-Hub-Signature-256: sha256=<hex>` over the raw body.
    Github,
    /// `Stripe-Signature: t=<unix>,v1=<hex>[,v1=<hex>...]` over `"{t}.{body}"`.
    Stripe,
}

impl Scheme {
    /// The (lowercase) header that carries the signature.
    pub fn header_name(self) -> &'static str {
        match self {
            Scheme::Github => "x-hub-signature-256",
            Scheme::Stripe => "stripe-signature",
        }
    }
}

impl FromStr for Scheme {
    type Err = UnknownScheme;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "github" => Ok(Scheme::Github),
            "stripe" => Ok(Scheme::Stripe),
            _ => Err(UnknownScheme(s.to_string())),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("unknown signature scheme `{0}` (expected `github` or `stripe`)")]
pub struct UnknownScheme(pub String);

/// Why a delivery was rejected. Never carries the secret or the signature itself.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VerifyError {
    #[error("missing signature header")]
    Missing,
    #[error("malformed signature header")]
    Malformed,
    #[error("signature timestamp outside tolerance")]
    Expired,
    #[error("signature mismatch")]
    Mismatch,
}

/// Verifies `body` against the signature `header` using `scheme`.
///
/// `now_unix` and `tolerance` only apply to schemes with a signed timestamp (Stripe).
pub fn verify(
    scheme: Scheme,
    secret: &[u8],
    header: Option<&str>,
    body: &[u8],
    now_unix: i64,
    tolerance: Duration,
) -> Result<(), VerifyError> {
    let header = header.ok_or(VerifyError::Missing)?;
    match scheme {
        Scheme::Github => verify_github(secret, header, body),
        Scheme::Stripe => verify_stripe(secret, header, body, now_unix, tolerance),
    }
}

/// Verifies a GitHub `X-Hub-Signature-256` header value (`sha256=<hex>`).
pub fn verify_github(secret: &[u8], header: &str, body: &[u8]) -> Result<(), VerifyError> {
    let hex_sig = header
        .trim()
        .strip_prefix("sha256=")
        .ok_or(VerifyError::Malformed)?;
    let signature = hex::decode(hex_sig).map_err(|_| VerifyError::Malformed)?;

    let mut mac = new_mac(secret);
    mac.update(body);
    mac.verify_slice(&signature)
        .map_err(|_| VerifyError::Mismatch)
}

/// Verifies a Stripe `Stripe-Signature` header value (`t=<unix>,v1=<hex>[,v1=<hex>...]`).
///
/// Accepts if any `v1` signature matches, then rejects if the timestamp is more than
/// `tolerance` away from `now_unix` in either direction. Other schemes (such as `v0`)
/// are ignored, as Stripe's own libraries do.
pub fn verify_stripe(
    secret: &[u8],
    header: &str,
    body: &[u8],
    now_unix: i64,
    tolerance: Duration,
) -> Result<(), VerifyError> {
    let mut timestamp = None;
    let mut signatures = Vec::new();
    for item in header.split(',') {
        let (key, value) = item.trim().split_once('=').ok_or(VerifyError::Malformed)?;
        match key {
            "t" if timestamp.is_none() => timestamp = Some(value),
            "t" => return Err(VerifyError::Malformed),
            "v1" => signatures.push(hex::decode(value).map_err(|_| VerifyError::Malformed)?),
            _ => {}
        }
    }
    let timestamp = timestamp.ok_or(VerifyError::Malformed)?;
    let signed_at: i64 = timestamp.parse().map_err(|_| VerifyError::Malformed)?;
    if signatures.is_empty() {
        return Err(VerifyError::Malformed);
    }

    // Stripe signs the timestamp exactly as it appears in the header.
    let mut mac = new_mac(secret);
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(body);
    if !signatures
        .iter()
        .any(|sig| mac.clone().verify_slice(sig).is_ok())
    {
        return Err(VerifyError::Mismatch);
    }

    // Only checked once the signature is known to be authentic, so the timestamp can be trusted.
    if now_unix.abs_diff(signed_at) > tolerance.as_secs() {
        return Err(VerifyError::Expired);
    }
    Ok(())
}

fn new_mac(secret: &[u8]) -> HmacSha256 {
    HmacSha256::new_from_slice(secret).expect("HMAC accepts keys of any length")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(secret: &[u8], payload: &[u8]) -> String {
        let mut mac = new_mac(secret);
        mac.update(payload);
        hex::encode(mac.finalize().into_bytes())
    }

    mod github {
        use super::*;

        // Test vector published in GitHub's "Validating webhook deliveries" docs.
        const SECRET: &[u8] = b"It's a Secret to Everybody";
        const BODY: &[u8] = b"Hello, World!";
        const HEADER: &str =
            "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";

        #[test]
        fn accepts_published_test_vector() {
            let result = verify_github(SECRET, HEADER, BODY);
            assert_eq!(result, Ok(()));
        }

        #[test]
        fn rejects_tampered_body() {
            let result = verify_github(SECRET, HEADER, b"Hello, World?");
            assert_eq!(result, Err(VerifyError::Mismatch));
        }

        #[test]
        fn rejects_wrong_secret() {
            let result = verify_github(b"not the secret", HEADER, BODY);
            assert_eq!(result, Err(VerifyError::Mismatch));
        }

        #[test]
        fn rejects_header_without_sha256_prefix() {
            let header = HEADER.trim_start_matches("sha256=");
            let result = verify_github(SECRET, header, BODY);
            assert_eq!(result, Err(VerifyError::Malformed));
        }

        #[test]
        fn rejects_non_hex_signature() {
            let result = verify_github(SECRET, "sha256=not-hex", BODY);
            assert_eq!(result, Err(VerifyError::Malformed));
        }

        #[test]
        fn rejects_truncated_signature() {
            let result = verify_github(SECRET, &HEADER[..HEADER.len() - 2], BODY);
            assert_eq!(result, Err(VerifyError::Mismatch));
        }

        #[test]
        fn rejects_empty_signature() {
            let result = verify_github(SECRET, "sha256=", BODY);
            assert_eq!(result, Err(VerifyError::Mismatch));
        }

        #[test]
        fn rejects_missing_header() {
            let result = verify(Scheme::Github, SECRET, None, BODY, 0, Duration::ZERO);
            assert_eq!(result, Err(VerifyError::Missing));
        }
    }

    mod stripe {
        use super::*;

        const SECRET: &[u8] = b"whsec_test_secret";
        const BODY: &[u8] = br#"{"id":"evt_1","type":"charge.succeeded"}"#;
        const NOW: i64 = 1_700_000_000;

        fn sig_at(t: i64, secret: &[u8], body: &[u8]) -> String {
            sign(secret, &[t.to_string().as_bytes(), b".", body].concat())
        }

        fn header_at(t: i64, secret: &[u8], body: &[u8]) -> String {
            format!("t={t},v1={}", sig_at(t, secret, body))
        }

        fn check(header: &str, body: &[u8]) -> Result<(), VerifyError> {
            verify_stripe(SECRET, header, body, NOW, DEFAULT_STRIPE_TOLERANCE)
        }

        #[test]
        fn accepts_valid_signature() {
            let result = check(&header_at(NOW, SECRET, BODY), BODY);
            assert_eq!(result, Ok(()));
        }

        #[test]
        fn rejects_tampered_body() {
            let result = check(&header_at(NOW, SECRET, BODY), b"{}");
            assert_eq!(result, Err(VerifyError::Mismatch));
        }

        #[test]
        fn rejects_wrong_secret() {
            let result = check(&header_at(NOW, b"whsec_other", BODY), BODY);
            assert_eq!(result, Err(VerifyError::Mismatch));
        }

        #[test]
        fn rejects_timestamp_swapped_after_signing() {
            let header = header_at(NOW, SECRET, BODY).replacen(&NOW.to_string(), "1700000001", 1);
            let result = check(&header, BODY);
            assert_eq!(result, Err(VerifyError::Mismatch));
        }

        #[test]
        fn rejects_expired_timestamp() {
            let result = check(&header_at(NOW - 301, SECRET, BODY), BODY);
            assert_eq!(result, Err(VerifyError::Expired));
        }

        #[test]
        fn rejects_timestamp_too_far_in_the_future() {
            let result = check(&header_at(NOW + 301, SECRET, BODY), BODY);
            assert_eq!(result, Err(VerifyError::Expired));
        }

        #[test]
        fn accepts_timestamp_at_tolerance_edge() {
            let result = check(&header_at(NOW - 300, SECRET, BODY), BODY);
            assert_eq!(result, Ok(()));
        }

        #[test]
        fn honors_custom_tolerance() {
            let header = header_at(NOW - 10, SECRET, BODY);
            let result = verify_stripe(SECRET, &header, BODY, NOW, Duration::from_secs(5));
            assert_eq!(result, Err(VerifyError::Expired));
        }

        #[test]
        fn accepts_when_any_v1_matches() {
            // Stripe sends several v1 values while a secret is being rolled.
            let valid = sig_at(NOW, SECRET, BODY);
            let stale = sig_at(NOW, b"whsec_old", BODY);
            let header = format!("t={NOW},v1={stale},v1={valid}");
            let result = check(&header, BODY);
            assert_eq!(result, Ok(()));
        }

        #[test]
        fn rejects_when_no_v1_matches() {
            let bogus = "0".repeat(64);
            let header = format!("t={NOW},v1={bogus},v1={bogus}");
            let result = check(&header, BODY);
            assert_eq!(result, Err(VerifyError::Mismatch));
        }

        #[test]
        fn rejects_empty_v1_signature() {
            let result = check(&format!("t={NOW},v1="), BODY);
            assert_eq!(result, Err(VerifyError::Mismatch));
        }

        #[test]
        fn ignores_other_schemes() {
            let header = format!("{},v0={}", header_at(NOW, SECRET, BODY), "0".repeat(64));
            let result = check(&header, BODY);
            assert_eq!(result, Ok(()));
        }

        #[test]
        fn rejects_header_without_v1() {
            let result = check(&format!("t={NOW},v0=abcd"), BODY);
            assert_eq!(result, Err(VerifyError::Malformed));
        }

        #[test]
        fn rejects_header_without_timestamp() {
            let header = header_at(NOW, SECRET, BODY).replacen(&format!("t={NOW},"), "", 1);
            let result = check(&header, BODY);
            assert_eq!(result, Err(VerifyError::Malformed));
        }

        #[test]
        fn rejects_non_numeric_timestamp() {
            let result = check(&format!("t=soon,v1={}", "0".repeat(64)), BODY);
            assert_eq!(result, Err(VerifyError::Malformed));
        }

        #[test]
        fn rejects_duplicate_timestamp() {
            let header = format!("t={NOW},{}", header_at(NOW, SECRET, BODY));
            let result = check(&header, BODY);
            assert_eq!(result, Err(VerifyError::Malformed));
        }

        #[test]
        fn rejects_item_without_equals_sign() {
            let result = check("garbage", BODY);
            assert_eq!(result, Err(VerifyError::Malformed));
        }
    }

    #[test]
    fn scheme_parses_case_insensitively() {
        assert_eq!("Stripe".parse::<Scheme>(), Ok(Scheme::Stripe));
    }

    #[test]
    fn scheme_rejects_unknown_names() {
        assert!("gitlab".parse::<Scheme>().is_err());
    }
}
