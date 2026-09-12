/// The current Codex CLI version as embedded at compile time.
pub const CODEX_CLI_VERSION: &str = env!("CARGO_PKG_VERSION");

// Keep layout fixtures independent of package releases without changing the
// version used for update decisions, tooltip eligibility, or client identity.
#[cfg(not(test))]
pub(crate) const CODEX_CLI_DISPLAY_VERSION: &str = CODEX_CLI_VERSION;
#[cfg(test)]
pub(crate) const CODEX_CLI_DISPLAY_VERSION: &str = "0.0.0";
