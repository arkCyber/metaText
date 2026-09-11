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
/// use meta_text::utils::bytes_to_hex;
///
/// let bytes = vec![0x01, 0x02, 0x03, 0x04];
/// let hex = bytes_to_hex(&bytes);
/// assert_eq!(hex, "01020304");
/// ```
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
/// # Examples
///
/// ```rust
/// use meta_text::utils::hex_to_bytes;
///
/// let hex = "01020304";
/// let bytes = hex_to_bytes(hex).unwrap();
/// assert_eq!(bytes, vec![0x01, 0x02, 0x03, 0x04]);
/// ```
pub fn hex_to_bytes(hex_string: &str) -> MetaTextResult<Vec<u8>> {
    hex::decode(hex_string).map_err(|e| MetaTextError::Validation {
        message: format!("Invalid hexadecimal string: {}", e),
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
/// use meta_text::utils::generate_random_string;
///
/// let random = generate_random_string(10);
/// assert_eq!(random.len(), 10);
/// ```
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
/// use meta_text::utils::get_timestamp;
///
/// let timestamp = get_timestamp();
/// assert!(!timestamp.is_empty());
/// ```
pub fn get_timestamp() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
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
/// use meta_text::utils::is_valid_email;
///
/// assert!(is_valid_email("user@example.com"));
/// assert!(!is_valid_email("invalid-email"));
/// ```
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
/// use meta_text::utils::truncate_string;
///
/// let result = truncate_string("Hello World", 8);
/// assert_eq!(result, "Hello...");
///
/// // Multi-byte input is truncated on a character boundary.
/// assert_eq!(truncate_string("你好世界再见", 4), "你...");
/// ```
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
/// # Examples
///
/// ```rust
/// use meta_text::utils::ensure_directory;
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     ensure_directory("logs").await?;
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
}
