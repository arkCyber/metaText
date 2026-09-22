/*!
 * utils.rs
 *
 * Utility functions for metaText messaging client
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 */

use tracing::info;

use crate::error::{MetaTextError, MetaTextResult};

/// Convert bytes to hexadecimal string
///
/// # Arguments
///
/// * `bytes` - The bytes to convert
///
/// # Returns
///
/// Returns a hexadecimal string representation of the bytes.
///
/// # Examples
///
/// ```rust
/// use meta_text_proto::utils::bytes_to_hex;
///
/// let bytes = vec![0x01, 0x02, 0x03, 0x04];
/// let hex = bytes_to_hex(&bytes);
/// assert_eq!(hex, "01020304");
/// ```
#[must_use]
pub fn bytes_to_hex(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

/// Convert hexadecimal string to bytes
///
/// # Arguments
///
/// * `hex_string` - The hexadecimal string to convert
///
/// # Returns
///
/// Returns the bytes represented by the hexadecimal string.
///
/// # Errors
///
/// Returns [`MetaTextError::Validation`] when `hex_string` is not a whole number
/// of hexadecimal digits, so a caller can report the offending field instead of
/// a parse error. An odd length is refused rather than padded.
///
/// # Examples
///
/// ```rust
/// use meta_text_proto::utils::hex_to_bytes;
///
/// let hex = "01020304";
/// let bytes = hex_to_bytes(hex).unwrap();
/// assert_eq!(bytes, vec![0x01, 0x02, 0x03, 0x04]);
/// ```
pub fn hex_to_bytes(hex_string: &str) -> MetaTextResult<Vec<u8>> {
    hex::decode(hex_string).map_err(|e| MetaTextError::Validation {
        message: format!("Invalid hexadecimal string: {e}"),
        field: "hex_string".to_string(),
        expected: Some("Valid hexadecimal string".to_string()),
    })
}

/// Generate a random string of specified length
///
/// # Arguments
///
/// * `length` - The length of the random string
///
/// # Returns
///
/// Returns a random string of the specified length.
///
/// # Examples
///
/// ```rust
/// use meta_text_proto::utils::generate_random_string;
///
/// let random = generate_random_string(10);
/// assert_eq!(random.len(), 10);
/// ```
#[must_use]
pub fn generate_random_string(length: usize) -> String {
    use rand::Rng;
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ\
                            abcdefghijklmnopqrstuvwxyz\
                            0123456789";
    let mut rng = rand::thread_rng();

    (0..length)
        .map(|_| {
            let idx = rng.gen_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

/// Get current timestamp as string
///
/// # Returns
///
/// Returns the current timestamp formatted as a string.
///
/// # Examples
///
/// ```rust
/// use meta_text_proto::utils::get_timestamp;
///
/// let timestamp = get_timestamp();
/// assert!(!timestamp.is_empty());
/// ```
#[must_use]
pub fn get_timestamp() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Check whether `address` looks like a usable `host:port` endpoint.
///
/// The check is deliberately structural rather than a DNS/IP parse: it must
/// accept host names, IPv4 and bracketed IPv6 alike, and it must reject the
/// ambiguous cases (empty host, port 0, more than a numeric tail).
///
/// # Arguments
///
/// * `address` - Candidate `host:port` string.
///
/// # Examples
///
/// ```rust
/// use meta_text_proto::utils::is_valid_host_port;
///
/// assert!(is_valid_host_port("127.0.0.1:33445"));
/// assert!(is_valid_host_port("tox.example.org:3389"));
/// assert!(is_valid_host_port("[::1]:33445"));
/// assert!(!is_valid_host_port("no-port"));
/// assert!(!is_valid_host_port("host:0"));
/// ```
#[must_use]
pub fn is_valid_host_port(address: &str) -> bool {
    let address = address.trim();

    let (host, port) = if let Some(rest) = address.strip_prefix('[') {
        // Bracketed IPv6 keeps its colons inside the brackets.
        match rest.split_once("]:") {
            Some(parts) => parts,
            None => return false,
        }
    } else {
        match address.rsplit_once(':') {
            // An unbracketed host may not itself contain a colon: that would be
            // an ambiguous IPv6 literal.
            Some((host, port)) if !host.contains(':') => (host, port),
            _ => return false,
        }
    };

    !host.is_empty() && port.parse::<u16>().is_ok_and(|value| value > 0)
}

/// Validate email address format
///
/// # Arguments
///
/// * `email` - The email address to validate
///
/// # Returns
///
/// Returns `true` if the email is valid, `false` otherwise.
///
/// # Examples
///
/// ```rust
/// use meta_text_proto::utils::is_valid_email;
///
/// assert!(is_valid_email("user@example.com"));
/// assert!(!is_valid_email("invalid-email"));
/// ```
#[must_use]
pub fn is_valid_email(email: &str) -> bool {
    // Basic email validation
    if !email.contains('@') || !email.contains('.') || email.len() < 5 {
        return false;
    }

    // Split by @ and check parts
    let parts: Vec<&str> = email.split('@').collect();
    if parts.len() != 2 {
        return false;
    }

    let username = parts[0];
    let domain = parts[1];

    // Check username is not empty
    if username.is_empty() {
        return false;
    }

    // Check domain has at least one dot and valid parts
    if !domain.contains('.') || domain.starts_with('.') || domain.ends_with('.') {
        return false;
    }

    true
}

/// Truncate string to specified length
///
/// The length is measured in `char`s (not bytes) so that multi-byte UTF-8
/// input such as CJK text or emoji is never split in the middle of a code
/// point, which would otherwise panic on the slice boundary.
///
/// # Arguments
///
/// * `s` - The string to truncate
/// * `max_length` - Maximum length in characters
///
/// # Returns
///
/// Returns the truncated string with a `"..."` suffix when it had to be cut.
/// When `max_length` is smaller than the length of the suffix the string is
/// simply cut to `max_length` characters without a suffix.
///
/// # Examples
///
/// ```rust
/// use meta_text_proto::utils::truncate_string;
///
/// let result = truncate_string("Hello World", 8);
/// assert_eq!(result, "Hello...");
///
/// // Multi-byte input is truncated on a character boundary.
/// assert_eq!(truncate_string("你好世界再见", 4), "你...");
/// ```
#[must_use]
pub fn truncate_string(s: &str, max_length: usize) -> String {
    if s.chars().count() <= max_length {
        return s.to_string();
    }

    // Not enough room for the ellipsis suffix: cut without a suffix.
    if max_length <= 3 {
        return s.chars().take(max_length).collect();
    }

    let truncated: String = s.chars().take(max_length - 3).collect();
    format!("{truncated}...")
}

/// Abbreviate a long identifier for display (`D5F0…0831`).
///
/// Keys and addresses are shown in full only when the user asks for them
/// (`/whoami`); everywhere else the first and last four characters are enough to
/// recognise a peer without wrapping the line.
///
/// # Examples
///
/// ```rust
/// use meta_text_proto::utils::abbreviate;
///
/// assert_eq!(abbreviate("D5F0CFAD57CC86F5"), "D5F0…86F5");
/// // Short values are returned unchanged instead of losing their middle.
/// assert_eq!(abbreviate("ABCD"), "ABCD");
/// assert_eq!(abbreviate(""), "");
/// ```
#[must_use]
pub fn abbreviate(value: &str) -> String {
    const EDGE: usize = 4;

    // Count characters rather than bytes so a multi-byte value cannot be cut
    // inside a code point.
    let characters: Vec<char> = value.chars().collect();
    if characters.len() <= EDGE * 2 {
        return value.to_string();
    }

    let head: String = characters[..EDGE].iter().collect();
    let tail: String = characters[characters.len() - EDGE..].iter().collect();
    format!("{head}…{tail}")
}

/// Whether a `host:port` bind address targets the loopback interface.
///
/// Used to decide whether an unauthenticated core protocol endpoint is
/// acceptable: loopback is reachable only from this machine, so a missing token
/// is a local trust decision; anything else is exposed to the network.
///
/// # Examples
///
/// ```rust
/// use meta_text_proto::utils::is_loopback_host;
///
/// assert!(is_loopback_host("127.0.0.1:45999"));
/// assert!(is_loopback_host("localhost:45999"));
/// assert!(is_loopback_host("[::1]:45999"));
/// assert!(!is_loopback_host("0.0.0.0:45999"));
/// assert!(!is_loopback_host("10.0.0.5:45999"));
/// ```
#[must_use]
pub fn is_loopback_host(address: &str) -> bool {
    let trimmed = address.trim();

    // A bare literal carries no port, so try it first: `::1` would otherwise be
    // split at its last colon and read as the host `:`.
    if let Ok(literal) = trimmed.parse::<std::net::IpAddr>() {
        return literal.is_loopback();
    }

    let Some(host) = host_of(trimmed) else {
        return false;
    };

    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }

    // `127.0.0.0/8` is loopback; `0.0.0.0`/`::` are "any interface" and are not.
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

/// The host part of `host`, `host:port`, `[v6]` or `[v6]:port`.
///
/// Split out of [`is_loopback_host`] because the two ways of stripping a port
/// (inside brackets, or up to the last colon) read better as early returns than
/// as one `Option`-returning expression; a bracketed value with no closing
/// bracket is not an address at all and yields `None`.
fn host_of(address: &str) -> Option<&str> {
    if let Some(rest) = address.strip_prefix('[') {
        return rest.split_once(']').map(|(host, _)| host);
    }

    // Without brackets the port is what follows the last colon; a value with no
    // colon is already the host.
    Some(address.rsplit_once(':').map_or(address, |(host, _)| host))
}

/// Create a directory if it doesn't exist
///
/// # Arguments
///
/// * `path` - The directory path to create
///
/// # Returns
///
/// Returns `Ok(())` if the directory was created or already exists.
///
/// # Errors
///
/// Returns [`MetaTextError::Configuration`] when the directory cannot be created
/// (a permission problem, or a path component that is a file). Callers that
/// cannot proceed without the directory should propagate it; the logging setup
/// treats it as fatal at startup on purpose, because a missing log directory
/// would otherwise only surface as unreadable logs much later.
///
/// # Examples
///
/// ```rust
/// use meta_text_proto::utils::ensure_directory;
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     // A throwaway directory on purpose: an example must not create a `logs`
///     // directory in whatever directory it happens to run in.
///     let dir = tempfile::tempdir()?;
///     ensure_directory(dir.path().join("logs")).await?;
///     Ok(())
/// }
/// ```
pub async fn ensure_directory<P: AsRef<std::path::Path>>(path: P) -> MetaTextResult<()> {
    let path = path.as_ref();

    if !path.exists() {
        tokio::fs::create_dir_all(path)
            .await
            .map_err(|e| MetaTextError::Configuration {
                message: format!("Failed to create directory {}: {}", path.display(), e),
                source: Some(Box::new(e)),
            })?;

        info!(
            "📁 [{}] Created directory: {}",
            get_timestamp(),
            path.display()
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bytes_to_hex() {
        let bytes = vec![0x01, 0x02, 0x03, 0x04];
        let hex = bytes_to_hex(&bytes);
        assert_eq!(hex, "01020304");
    }

    #[test]
    fn test_hex_to_bytes() {
        let hex = "01020304";
        let bytes = hex_to_bytes(hex).unwrap();
        assert_eq!(bytes, vec![0x01, 0x02, 0x03, 0x04]);
    }

    #[test]
    fn test_hex_to_bytes_invalid() {
        let hex = "invalid";
        let result = hex_to_bytes(hex);
        assert!(result.is_err());
    }

    #[test]
    fn test_generate_random_string() {
        let random = generate_random_string(10);
        assert_eq!(random.len(), 10);

        let random2 = generate_random_string(10);
        assert_eq!(random2.len(), 10);
        // Note: These might be the same by chance, but very unlikely
    }

    #[test]
    fn test_get_timestamp() {
        let timestamp = get_timestamp();
        assert!(!timestamp.is_empty());
        assert!(timestamp.contains('-'));
        assert!(timestamp.contains(':'));
    }

    #[test]
    fn test_is_valid_email() {
        assert!(is_valid_email("user@example.com"));
        assert!(is_valid_email("test.user@domain.co.uk"));
        assert!(!is_valid_email("invalid-email"));
        assert!(!is_valid_email("user@"));
        assert!(!is_valid_email("@domain.com"));
    }

    #[test]
    fn test_truncate_string() {
        assert_eq!(truncate_string("Hello World", 8), "Hello...");
        assert_eq!(truncate_string("Short", 10), "Short");
        assert_eq!(truncate_string("", 5), "");
    }

    #[test]
    fn test_truncate_string_is_utf8_safe() {
        // Multi-byte characters must be truncated on a character boundary
        // instead of panicking on a byte slice.
        assert_eq!(truncate_string("你好世界再见", 4), "你...");
        assert_eq!(truncate_string("😀😀😀", 2), "😀😀");
        // Not enough room for the suffix: cut without adding "...".
        assert_eq!(truncate_string("你好世界", 1), "你");
        assert_eq!(truncate_string("abcdef", 3), "abc");
        // Exactly at the character limit stays untouched.
        assert_eq!(truncate_string("你好世界", 4), "你好世界");
    }

    /// Host:port validation accepts the shapes peers actually use and rejects
    /// the ambiguous ones.
    #[test]
    fn test_is_valid_host_port() {
        // Accepted
        assert!(is_valid_host_port("127.0.0.1:33445"));
        assert!(is_valid_host_port("localhost:1"));
        assert!(is_valid_host_port("tox.abilinski.com:3389"));
        assert!(is_valid_host_port("[::1]:33445"));
        assert!(
            is_valid_host_port("  host:33445  "),
            "surrounding space is trimmed"
        );

        // Rejected
        assert!(!is_valid_host_port(""));
        assert!(!is_valid_host_port("host"));
        assert!(!is_valid_host_port(":33445"), "empty host");
        assert!(!is_valid_host_port("host:"), "empty port");
        assert!(!is_valid_host_port("host:0"), "port 0 is not dialable");
        assert!(!is_valid_host_port("host:99999"), "port out of range");
        assert!(!is_valid_host_port("host:abc"), "non numeric port");
        assert!(!is_valid_host_port("::1:33445"), "unbracketed IPv6");
        assert!(
            !is_valid_host_port("[::1]33445"),
            "missing bracket separator"
        );
        assert!(!is_valid_host_port("[::1]:"), "empty port after bracket");
    }

    #[tokio::test]
    async fn test_ensure_directory() {
        let test_dir = "test_dir";

        // Clean up from previous test runs
        let _ = tokio::fs::remove_dir_all(test_dir).await;

        // Create directory
        ensure_directory(test_dir).await.unwrap();
        assert!(std::path::Path::new(test_dir).exists());

        // Try to create again (should not fail)
        ensure_directory(test_dir).await.unwrap();

        // Clean up
        tokio::fs::remove_dir_all(test_dir).await.unwrap();
    }

    /// Loopback detection drives the "no token on a public interface" rule, so
    /// every shape (IPv4, IPv6, name, wildcard) is pinned here.
    #[test]
    fn test_is_loopback_host() {
        // Loopback in its many spellings.
        for address in [
            "127.0.0.1:45999",
            "127.0.0.5:1",
            "localhost:45999",
            "LOCALHOST:45999",
            "[::1]:45999",
            "127.0.0.1",
            "::1",
        ] {
            assert!(is_loopback_host(address), "'{address}' is loopback");
        }

        // Anything reachable from another host is not.
        for address in [
            "0.0.0.0:45999",
            "[::]:45999",
            "10.0.0.5:45999",
            "192.168.1.10:45999",
            "example.org:45999",
            "[2001:db8::1]:45999",
        ] {
            assert!(!is_loopback_host(address), "'{address}' is not loopback");
        }
    }

    /// `abbreviate` keeps short values intact and never splits a code point.
    #[test]
    fn test_abbreviate() {
        assert_eq!(abbreviate(""), "");
        assert_eq!(abbreviate("ABCD"), "ABCD");
        assert_eq!(abbreviate("ABCDEFGH"), "ABCDEFGH");
        assert_eq!(abbreviate("ABCDEFGHI"), "ABCD…FGHI");
        assert_eq!(
            abbreviate(&"A".repeat(76)),
            format!("AAAA…{}", "A".repeat(4))
        );
        // Multi-byte input is cut on character boundaries.
        assert_eq!(abbreviate("你好世界一二三四五"), "你好世界…二三四五");
    }
}
