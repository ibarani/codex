# Provider model metadata discovery

`ModelProviderInfo.model_discovery` controls optional remote metadata acquisition,
independently of the provider's authentication method:

- `auto` is the default and retains the existing authentication-based discovery
  policy and cache behavior.
- `disabled` uses bundled native metadata and explicit model settings. It skips
  remote discovery, authentication for discovery, and all remote-model cache
  operations, including TTL renewal on an unchanged ETag.

For a provider exposing a standard model inventory instead of Codex's rich catalog,
set `model_discovery = "disabled"` in that provider's configuration. With bundled
fallback metadata, explicit model identity, context overrides and native instruction
templates are preserved. An explicitly supplied authoritative local model catalog
retains its existing selection and instruction semantics. Ordinary inference
authentication is unchanged. Missing rich metadata remains fallback metadata;
this setting does not invent instructions, capabilities or an advertised maximum.

The remote thread-config protocol carries the same policy as enum field 19.
Omitted legacy values mean `auto`; unknown values fail parsing. Clients predating
this field cannot apply the new policy. The removed `remote_models` feature flag
remains a compatibility no-op and is not this setting.

Rich-catalog decoding still requires its instruction contract. Decode diagnostics
report category, line and column, excluding response bodies and offending values.
Standard-inventory discovery and cache partitioning between enabled providers are
separate requirements; disabling discovery does not claim to implement either.

Behavioral coverage lives in the models-manager discovery tests, provider factory
and endpoint tests, provider TOML tests, and remote thread-config conversion tests.
