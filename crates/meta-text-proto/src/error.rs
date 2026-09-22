/*!
 * error.rs
 *
 * Error handling and custom error types for metaText messaging client
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - Custom error types for different application domains
 * - Error conversion implementations
 * - Structured error reporting
 * - Error context preservation
 */

use thiserror::Error;
use tracing::{error, warn};

/// Custom error types for the metaText application
///
/// Provides domain-specific error types that can be easily converted
/// to and from other error types, with rich context information
/// for debugging and user feedback.
#[derive(Error, Debug)]
pub enum MetaTextError {
    /// Configuration-related errors
    #[error("Configuration error: {message}")]
    Configuration {
        /// Error message
        message: String,
        /// Source error if available
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    /// Network communication errors
    #[error("Network error: {message}")]
    Network {
        /// Error message
        message: String,
        /// Network operation that failed
        operation: String,
        /// Source error if available
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    /// Cryptographic operation errors
    #[error("Cryptographic error: {message}")]
    Cryptographic {
        /// Error message
        message: String,
        /// Cryptographic operation that failed
        operation: String,
        /// Source error if available
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    /// Database operation errors
    #[error("Database error: {message}")]
    Database {
        /// Error message
        message: String,
        /// Database operation that failed
        operation: String,
        /// Source error if available
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    /// User interface errors
    #[error("UI error: {message}")]
    UserInterface {
        /// Error message
        message: String,
        /// UI component that failed
        component: String,
        /// Source error if available
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    /// Message processing errors
    #[error("Message error: {message}")]
    Message {
        /// Error message
        message: String,
        /// Message ID if available
        message_id: Option<String>,
        /// Source error if available
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    /// Authentication and authorization errors
    #[error("Authentication error: {message}")]
    Authentication {
        /// Error message
        message: String,
        /// Authentication method that failed
        method: String,
        /// Source error if available
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    /// Resource exhaustion errors
    #[error("Resource error: {message}")]
    Resource {
        /// Error message
        message: String,
        /// Resource type that was exhausted
        resource_type: String,
        /// Source error if available
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    /// Validation errors for user input
    #[error("Validation error: {message}")]
    Validation {
        /// Error message
        message: String,
        /// Field that failed validation
        field: String,
        /// Expected value or format
        expected: Option<String>,
    },

    /// Internal application errors
    #[error("Internal error: {message}")]
    Internal {
        /// Error message
        message: String,
        /// Component where the error occurred
        component: String,
        /// Source error if available
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },
}

impl MetaTextError {
    /// Check if this error is critical and requires application shutdown
    ///
    /// # Returns
    ///
    /// Returns `true` if the error is critical and should trigger
    /// application shutdown, `false` otherwise.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use meta_text_proto::error::MetaTextError;
    ///
    /// let error = MetaTextError::Resource {
    ///     message: "Out of memory".to_string(),
    ///     resource_type: "Memory".to_string(),
    ///     source: None,
    /// };
    ///
    /// assert!(error.is_critical());
    /// ```
    #[must_use]
    pub const fn is_critical(&self) -> bool {
        matches!(self, Self::Resource { .. } | Self::Internal { .. })
    }

    /// Get a user-friendly error message
    ///
    /// # Returns
    ///
    /// Returns a user-friendly error message that can be displayed
    /// to end users without technical details.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use meta_text_proto::error::MetaTextError;
    ///
    /// let error = MetaTextError::Network {
    ///     message: "Connection timeout".to_string(),
    ///     operation: "connect".to_string(),
    ///     source: None,
    /// };
    ///
    /// assert_eq!(error.user_message(), "Network connection failed. Please check your internet connection.");
    /// ```
    #[must_use]
    pub const fn user_message(&self) -> &'static str {
        match self {
            Self::Configuration { .. } => "Configuration error. Please check your settings.",
            Self::Network { .. } => {
                "Network connection failed. Please check your internet connection."
            }
            Self::Cryptographic { .. } => "Security error. Please restart the application.",
            Self::Database { .. } => "Data storage error. Please check available disk space.",
            Self::UserInterface { .. } => "Interface error. Please restart the application.",
            Self::Message { .. } => "Message processing error. Please try again.",
            Self::Authentication { .. } => "Authentication failed. Please check your credentials.",
            Self::Resource { .. } => "System resource error. Please restart the application.",
            Self::Validation { .. } => "Invalid input. Please check your data and try again.",
            Self::Internal { .. } => "Internal error. Please restart the application.",
        }
    }

    /// Log the error with appropriate level and context
    ///
    /// Automatically logs the error with the appropriate log level
    /// based on the error type and severity.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use meta_text_proto::error::MetaTextError;
    ///
    /// let error = MetaTextError::Network {
    ///     message: "Connection failed".to_string(),
    ///     operation: "connect".to_string(),
    ///     source: None,
    /// };
    ///
    /// error.log_error();
    /// ```
    pub fn log_error(&self) {
        let timestamp = chrono::Utc::now();

        match self {
            Self::Resource { .. } | Self::Internal { .. } => {
                error!(
                    "❌ [{}] Critical error: {:?}",
                    timestamp.format("%Y-%m-%d %H:%M:%S"),
                    self
                );
            }
            Self::Network { .. } | Self::Database { .. } => {
                error!(
                    "🌐 [{}] System error: {:?}",
                    timestamp.format("%Y-%m-%d %H:%M:%S"),
                    self
                );
            }
            Self::Cryptographic { .. } | Self::Authentication { .. } => {
                error!(
                    "🔐 [{}] Security error: {:?}",
                    timestamp.format("%Y-%m-%d %H:%M:%S"),
                    self
                );
            }
            _ => {
                warn!(
                    "⚠️ [{}] Application error: {:?}",
                    timestamp.format("%Y-%m-%d %H:%M:%S"),
                    self
                );
            }
        }
    }
}

/// Result type alias for metaText operations
///
/// Provides a convenient type alias for operations that can fail
/// with a `MetaTextError`.
pub type MetaTextResult<T> = Result<T, MetaTextError>;

/// Error context builder for adding context to errors
///
/// Provides a fluent interface for building error context
/// and converting other error types to `MetaTextError`.
#[derive(Debug)]
pub struct ErrorContext {
    /// Error message
    message: String,
    /// Additional context information
    context: std::collections::HashMap<String, String>,
}

impl ErrorContext {
    /// Create a new error context builder
    ///
    /// # Arguments
    ///
    /// * `message` - Base error message
    ///
    /// # Returns
    ///
    /// Returns a new `ErrorContext` builder.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use meta_text_proto::error::ErrorContext;
    ///
    /// let context = ErrorContext::new("Operation failed");
    /// ```
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            context: std::collections::HashMap::new(),
        }
    }

    /// Add context information to the error
    ///
    /// # Arguments
    ///
    /// * `key` - Context key
    /// * `value` - Context value
    ///
    /// # Returns
    ///
    /// Returns `self` for method chaining.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use meta_text_proto::error::ErrorContext;
    ///
    /// let context = ErrorContext::new("Network error")
    ///     .with_context("operation", "connect")
    ///     .with_context("peer", "example.com");
    /// ```
    #[must_use]
    pub fn with_context(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.context.insert(key.into(), value.into());
        self
    }

    /// Build a configuration error
    ///
    /// # Returns
    ///
    /// Returns a `MetaTextError::Configuration` with the built context.
    #[must_use]
    pub fn configuration_error(self) -> MetaTextError {
        MetaTextError::Configuration {
            message: self.message,
            source: None,
        }
    }

    /// Build a network error
    ///
    /// # Returns
    ///
    /// Returns a `MetaTextError::Network` with the built context.
    #[must_use]
    pub fn network_error(self) -> MetaTextError {
        let operation = self.context.get("operation").cloned().unwrap_or_default();
        MetaTextError::Network {
            message: self.message,
            operation,
            source: None,
        }
    }

    /// Build a cryptographic error
    ///
    /// # Returns
    ///
    /// Returns a `MetaTextError::Cryptographic` with the built context.
    #[must_use]
    pub fn cryptographic_error(self) -> MetaTextError {
        let operation = self.context.get("operation").cloned().unwrap_or_default();
        MetaTextError::Cryptographic {
            message: self.message,
            operation,
            source: None,
        }
    }

    /// Build a database error
    ///
    /// # Returns
    ///
    /// Returns a `MetaTextError::Database` with the built context.
    #[must_use]
    pub fn database_error(self) -> MetaTextError {
        let operation = self.context.get("operation").cloned().unwrap_or_default();
        MetaTextError::Database {
            message: self.message,
            operation,
            source: None,
        }
    }
}

// Conversion implementations for common error types

impl From<std::io::Error> for MetaTextError {
    fn from(err: std::io::Error) -> Self {
        Self::Internal {
            message: format!("I/O error: {err}"),
            component: "FileSystem".to_string(),
            source: Some(Box::new(err)),
        }
    }
}

impl From<serde_json::Error> for MetaTextError {
    fn from(err: serde_json::Error) -> Self {
        Self::Configuration {
            message: format!("JSON parsing error: {err}"),
            source: Some(Box::new(err)),
        }
    }
}

#[cfg(any(feature = "sqlite", feature = "postgres", feature = "mysql"))]
impl From<sqlx::Error> for MetaTextError {
    fn from(err: sqlx::Error) -> Self {
        Self::Database {
            message: format!("Database error: {err}"),
            operation: "query".to_string(),
            source: Some(Box::new(err)),
        }
    }
}

impl From<tokio::task::JoinError> for MetaTextError {
    fn from(err: tokio::task::JoinError) -> Self {
        Self::Internal {
            message: format!("Task join error: {err}"),
            component: "AsyncRuntime".to_string(),
            source: Some(Box::new(err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test error criticality detection
    #[test]
    fn test_error_criticality() {
        let resource_error = MetaTextError::Resource {
            message: "Out of memory".to_string(),
            resource_type: "Memory".to_string(),
            source: None,
        };
        assert!(resource_error.is_critical());

        let network_error = MetaTextError::Network {
            message: "Connection failed".to_string(),
            operation: "connect".to_string(),
            source: None,
        };
        assert!(!network_error.is_critical());
    }

    /// Test user-friendly error messages
    #[test]
    fn test_user_messages() {
        let network_error = MetaTextError::Network {
            message: "Connection timeout".to_string(),
            operation: "connect".to_string(),
            source: None,
        };

        assert_eq!(
            network_error.user_message(),
            "Network connection failed. Please check your internet connection."
        );
    }

    /// Test error context builder
    #[test]
    fn test_error_context_builder() {
        let context = ErrorContext::new("Network operation failed")
            .with_context("operation", "connect")
            .with_context("peer", "example.com");

        let network_error = context.network_error();

        match network_error {
            MetaTextError::Network {
                message, operation, ..
            } => {
                assert_eq!(message, "Network operation failed");
                assert_eq!(operation, "connect");
            }
            _ => panic!("Expected Network error"),
        }
    }

    /// Test error conversion from `std::io::Error`
    #[test]
    fn test_io_error_conversion() {
        let io_error = std::io::Error::new(std::io::ErrorKind::NotFound, "File not found");
        let meta_error: MetaTextError = io_error.into();

        match meta_error {
            MetaTextError::Internal {
                message, component, ..
            } => {
                assert!(message.contains("I/O error"));
                assert_eq!(component, "FileSystem");
            }
            _ => panic!("Expected Internal error"),
        }
    }
}
