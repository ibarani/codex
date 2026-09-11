//! Shared finite admission limits for one local diagnostic capture.
//!
//! These bound capture storage, never the model request. Oversized evidence is
//! rejected whole; the native session can continue with an incomplete capture.

pub(crate) const MAX_RECORD_BYTES: usize = 32 * 1024 * 1024;
pub(crate) const MAX_BUNDLE_BYTES: usize = 256 * 1024 * 1024;
pub(crate) const MAX_EVENT_COUNT: u64 = 65_536;
pub(crate) const MAX_PAYLOAD_COUNT: u64 = 4_096;
