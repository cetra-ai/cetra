# Licence fixtures

`dev-1.key` is the **development** ed25519 private key (32-byte seed, standard base64). It is
public on purpose: only debug builds trust its public half (`DEV_KEYS` in
`crates/token/src/keys.rs`, D-P-6). Release builds trust only the release keys there
(`prod-1`, `license-1`, `license-2` for licences), whose private keys are never in this repository.

`payloads/*.json` are token payloads; `tokens/*.token` are those payloads signed with
`dev-1` by `just license-fixtures` (P3). Do not edit `tokens/` by hand.
