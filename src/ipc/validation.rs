/*!
 * validation.rs
 *
 * Boundary validation for every value that enters the core from a front-end.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - One place where field limits and character rules are enforced
 * - Lengths are counted in `char`s so multi-byte input is measured correctly
 * - Control characters are rejected where they have no business being
 * - Every failure is a typed [`ErrorInfo`], never a panic
 *
 * # Rationale
 *
 * A front-end is an untrusted input source as far as the core is concerned: it
 * may be a script, a socket client or a future embedding host. Validating at
 * the boundary means the domain layer can assume its invariants hold, which is
 * what makes the actor's state predictable.
 */

use super::protocol::{
    contains_forbidden_control, ErrorCode, ErrorInfo, ALLOWED_MESSAGE_CONTROLS, MAX_IDENTIFIER_LEN,
    MAX_NICKNAME_LEN, MAX_NOTE_LEN, MAX_STATUS_LEN,
};

/// Build an `invalid_request` error mentioning the offending field.
fn invalid(field: &str, reason: impl std::fmt::Display) -> ErrorInfo {
    ErrorInfo::new(ErrorCode::InvalidRequest, format!("{field} {reason}"))
}

/// Validate and normalise a nickname.
///
/// # Errors
///
/// Returns [`ErrorCode::InvalidRequest`] when the trimmed value is empty, too
/// long or contains a control character.
pub fn nickname(value: &str) -> Result<String, ErrorInfo> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(invalid("nickname", "cannot be empty"));
    }
    let length = trimmed.chars().count();
    if length > MAX_NICKNAME_LEN {
        return Err(invalid(
            "nickname",
            format!("is {length} characters, the limit is {MAX_NICKNAME_LEN}"),
        ));
    }
    if contains_forbidden_control(trimmed, &[]) {
        return Err(invalid("nickname", "must not contain control characters"));
    }
    Ok(trimmed.to_string())
}

/// Validate and normalise a status message.
///
/// # Errors
///
/// Returns [`ErrorCode::InvalidRequest`] when the trimmed value is too long or
/// contains a control character.
pub fn status(value: &str) -> Result<String, ErrorInfo> {
    let trimmed = value.trim();
    let length = trimmed.chars().count();
    if length > MAX_STATUS_LEN {
        return Err(invalid(
            "status message",
            format!("is {length} characters, the limit is {MAX_STATUS_LEN}"),
        ));
    }
    if contains_forbidden_control(trimmed, &[]) {
        return Err(invalid(
            "status message",
            "must not contain control characters",
        ));
    }
    Ok(trimmed.to_string())
}

/// Validate and normalise a contact identifier.
///
/// Identifiers are DID-like opaque tokens, so internal whitespace is rejected:
/// it would break `/msg <name> <text>` and the nickname matching used by the
/// transport.
///
/// # Errors
///
/// Returns [`ErrorCode::InvalidRequest`] when the value is empty, too long,
/// contains whitespace or a control character.
pub fn identifier(value: &str) -> Result<String, ErrorInfo> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(invalid("contact identifier", "cannot be empty"));
    }
    let length = trimmed.chars().count();
    if length > MAX_IDENTIFIER_LEN {
        return Err(invalid(
            "contact identifier",
            format!("is {length} characters, the limit is {MAX_IDENTIFIER_LEN}"),
        ));
    }
    if trimmed.chars().any(char::is_whitespace) {
        return Err(invalid("contact identifier", "must not contain whitespace"));
    }
    if contains_forbidden_control(trimmed, &[]) {
        return Err(invalid(
            "contact identifier",
            "must not contain control characters",
        ));
    }
    Ok(trimmed.to_string())
}

/// Validate and normalise an optional contact note.
///
/// # Errors
///
/// Returns [`ErrorCode::InvalidRequest`] when the note is too long or contains
/// a control character.
pub fn note(value: Option<String>) -> Result<Option<String>, ErrorInfo> {
    let Some(value) = value else {
        return Ok(None);
    };

    let trimmed = value.trim();
    if trimmed.is_empty() {
        // An empty note is the same as no note.
        return Ok(None);
    }
    let length = trimmed.chars().count();
    if length > MAX_NOTE_LEN {
        return Err(invalid(
            "contact note",
            format!("is {length} characters, the limit is {MAX_NOTE_LEN}"),
        ));
    }
    if contains_forbidden_control(trimmed, &[]) {
        return Err(invalid(
            "contact note",
            "must not contain control characters",
        ));
    }
    Ok(Some(trimmed.to_string()))
}

/// Validate a message body.
///
/// # Arguments
///
/// * `value` - Raw body as sent by the front-end.
/// * `max_length` - Configured limit, in `char`s.
///
/// # Errors
///
/// Returns [`ErrorCode::InvalidRequest`] when the body is empty, exceeds
/// `max_length` characters, or contains a control character other than a
/// newline or a tab.
pub fn message(value: &str, max_length: usize) -> Result<String, ErrorInfo> {
    if value.trim().is_empty() {
        return Err(invalid("message", "cannot be empty"));
    }

    let length = value.chars().count();
    if length > max_length {
        return Err(invalid(
            "message",
            format!("is {length} characters, the limit is {max_length}"),
        ));
    }
    if contains_forbidden_control(value, &ALLOWED_MESSAGE_CONTROLS) {
        return Err(invalid("message", "must not contain control characters"));
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nicknames are trimmed and bounded.
    #[test]
    fn test_nickname_rules() {
        assert_eq!(nickname("  Alice  ").expect("valid"), "Alice");
        assert_eq!(
            nickname(&"a".repeat(MAX_NICKNAME_LEN))
                .expect("at limit")
                .len(),
            MAX_NICKNAME_LEN
        );

        assert_eq!(
            nickname("   ").expect_err("empty").code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            nickname(&"a".repeat(MAX_NICKNAME_LEN + 1))
                .expect_err("over limit")
                .code,
            ErrorCode::InvalidRequest
        );
        assert!(nickname("bad\u{0}name").is_err(), "NUL must be rejected");
        assert!(nickname("bad\u{7}name").is_err(), "BEL must be rejected");
    }

    /// The length check counts characters, not bytes.
    #[test]
    fn test_limits_are_measured_in_characters() {
        // 64 CJK characters are 192 bytes but exactly at the limit.
        let multibyte = "你".repeat(MAX_NICKNAME_LEN);
        assert_eq!(multibyte.len(), MAX_NICKNAME_LEN * 3);
        assert!(nickname(&multibyte).is_ok());

        let over = "你".repeat(MAX_NICKNAME_LEN + 1);
        assert!(nickname(&over).is_err());
    }

    /// Status messages allow spaces but no control characters.
    #[test]
    fn test_status_rules() {
        assert_eq!(
            status("  busy right now ").expect("valid"),
            "busy right now"
        );
        assert!(status("").is_ok(), "an empty status clears the field");
        assert!(status(&"s".repeat(MAX_STATUS_LEN)).is_ok());
        assert!(status(&"s".repeat(MAX_STATUS_LEN + 1)).is_err());
        assert!(status("bad\u{1b}[31m").is_err(), "escape must be rejected");
    }

    /// Identifiers are single tokens.
    #[test]
    fn test_identifier_rules() {
        assert_eq!(identifier("  DID123  ").expect("valid"), "DID123");
        assert!(identifier("").is_err());
        assert!(identifier("   ").is_err());
        assert!(identifier("two words").is_err(), "whitespace is rejected");
        assert!(identifier("tab\there").is_err());
        assert!(identifier(&"x".repeat(MAX_IDENTIFIER_LEN)).is_ok());
        assert!(identifier(&"x".repeat(MAX_IDENTIFIER_LEN + 1)).is_err());
        assert!(identifier("nul\u{0}").is_err());
    }

    /// Notes are optional and normalised.
    #[test]
    fn test_note_rules() {
        assert_eq!(note(None).expect("none"), None);
        assert_eq!(note(Some("   ".to_string())).expect("blank"), None);
        assert_eq!(
            note(Some("  met in the metaverse ".to_string())).expect("valid"),
            Some("met in the metaverse".to_string())
        );
        assert!(note(Some("n".repeat(MAX_NOTE_LEN + 1))).is_err());
        assert!(note(Some("bad\u{0}".to_string())).is_err());
    }

    /// Messages allow newlines and tabs but nothing else control-ish.
    #[test]
    fn test_message_rules() {
        assert_eq!(message("hello", 100).expect("valid"), "hello");
        assert!(message("line one\nline two", 100).is_ok());
        assert!(message("col1\tcol2", 100).is_ok());

        assert!(message("   ", 100).is_err(), "blank is rejected");
        assert!(message("hello", 4).is_err(), "over the limit");
        assert!(message("hello", 5).is_ok(), "exactly at the limit");
        assert!(message("nul\u{0}byte", 100).is_err());
        assert!(message("esc\u{1b}[2J", 100).is_err());
        assert!(message("bell\u{7}", 100).is_err());
    }
}
