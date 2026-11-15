# S1 — An `[[auth.oidc]]` client secret from the environment (R16)

**Commit subject:** `Read an OIDC provider's client secret from the environment`

**Goal:** `LLMOBS_AUTH__OIDC__<NAME>__CLIENT_SECRET` sets the `client_secret` of the `[[auth.oidc]]` provider named `<name>` (D-S-2). The provider stays in `llmobs.toml`, where `client_secret` becomes optional. Any other `LLMOBS_AUTH__OIDC__*` variable, and one that names no provider, stops the load with an error that names the variable and never its value.

## Read first

- README § Decisions D-S-2.
- `crates/server/src/config.rs`, all of it: `ALIASES`, `Secret` and its `Deserialize`, `AuthSettings` (doc comment lines "GitHub and Google accept the split env form … `LLMOBS_AUTH__OIDC__*` is ignored."), `OidcAuth`, `load_from`, `map_figment`, `secret_key`, `validate_auth`, `validate_oidc`, `EnvSource` and its `data`, `split_key`, `is_filtered`, `alias_key`, and in `mod tests`: `github_client_secret_stays_a_string_and_oidc_env_is_ignored`, `assert_rejected`, `oidc`, `PROVIDERS_TOML`.
- `docs/spec/12-operations.md` § Configuration (the paragraph "GitHub and Google also accept the split form …").
- `docs/spec/08-auth.md` § OAuth / OIDC (the TOML block).
- `docs/self-hosting/configuration.md` § Secrets.

**Check first:** `EnvSource::data` in `config.rs` contains the comment `` // `[[auth.oidc]]` is a TOML array. The split form cannot name an `` and the `continue` after it. If not, stop and report.

## Files

| File | Change |
|---|---|
| `crates/server/src/config.rs` | `client_secret` optional in TOML; `apply_oidc_env`; called from `load_from`; tests |
| `docs/spec/12-operations.md` | the env form |
| `docs/spec/08-auth.md` | the env form, in § OAuth / OIDC |
| `docs/self-hosting/configuration.md` | the env form, in § Secrets |
| `CHANGELOG.md` | `Added` and `Upgrade notes` lines |

## Steps

### 1. `OidcAuth.client_secret` becomes optional in TOML

In `config.rs`, on the field `client_secret` of `OidcAuth`, change the doc comment and add a serde default:

```rust
    /// OAuth client secret. Optional in the file when
    /// `LLMOBS_AUTH__OIDC__<NAME>__CLIENT_SECRET` sets it (D-S-2); required after loading.
    #[serde(default = "empty_secret")]
    pub client_secret: Secret<String>,
```

Add next to `default_oidc_scopes`:

```rust
fn empty_secret() -> Secret<String> {
    Secret(String::new())
}
```

`validate_oidc` already refuses an empty secret with `auth.oidc.<name>.client_secret: client secret is required`; keep that check unchanged.

### 2. `oidc_env_name` and `apply_oidc_env`

Add these two functions right after `load_from`:

```rust
/// The variable that sets provider `name`'s client secret (D-S-2): `cloud` →
/// `LLMOBS_AUTH__OIDC__CLOUD__CLIENT_SECRET`, `my-idp` → `…__MY_IDP__CLIENT_SECRET`.
fn oidc_env_name(name: &str) -> String {
    format!(
        "LLMOBS_AUTH__OIDC__{}__CLIENT_SECRET",
        name.to_ascii_uppercase().replace('-', "_")
    )
}

/// Applies `LLMOBS_AUTH__OIDC__<NAME>__CLIENT_SECRET` to the `[[auth.oidc]]` provider of
/// that name (D-S-2). Any other `LLMOBS_AUTH__OIDC__*` variable, and one naming no
/// provider, is an error that names the variable, never its value. Names compare without
/// regard to case, like every `LLMOBS_` variable.
fn apply_oidc_env(auth: &mut AuthSettings, env: &[(&str, &str)]) -> Result<(), ConfigError> {
    for (name, value) in env {
        let Some(dotted) = split_key(name) else {
            continue;
        };
        if dotted != "auth.oidc" && !dotted.starts_with("auth.oidc.") {
            continue;
        }
        let provider = auth
            .oidc
            .iter_mut()
            .find(|provider| name.eq_ignore_ascii_case(&oidc_env_name(&provider.name)));
        let Some(provider) = provider else {
            return Err(ConfigError::Provider {
                key: name.to_ascii_uppercase(),
                reason: "only LLMOBS_AUTH__OIDC__<NAME>__CLIENT_SECRET is read, for a provider of the config file",
            });
        };
        provider.client_secret = Secret((*value).to_owned());
    }
    Ok(())
}
```

Notes for the implementer:
- `split_key` returns `None` for names that are not `LLMOBS_…` or are filtered; it lower-cases and joins the parts with `.`, so `LLMOBS_AUTH__OIDC__CLOUD__CLIENT_SECRET` gives `auth.oidc.cloud.client_secret`. Do not change `split_key`.
- The error key is the variable's name (not secret: it holds the provider name). `ConfigError::Provider`'s `key` is a `String`, so this needs no new variant. Its doc comment says "Dotted path"; change that line to `/// Dotted path, for example \`auth.oidc.okta.issuer\`, or the name of the environment variable at fault.`
- A provider whose `name` is invalid (uppercase, `_`, too long) still gets its variable applied when one matches; `validate_oidc` rejects the name right after. That is fine.

### 3. Call it from `load_from`

In `load_from`, replace

```rust
    let mut config: ServerConfig = figment.extract().map_err(map_figment)?;
    validate(&mut config)?;
```

with

```rust
    let mut config: ServerConfig = figment.extract().map_err(map_figment)?;
    apply_oidc_env(&mut config.auth, env)?;
    validate(&mut config)?;
```

Keep the `continue` in `EnvSource::data` for `auth.oidc` (figment cannot merge into an array element); update its comment to:

```rust
                // `[[auth.oidc]]` is a TOML array, which the split form cannot index.
                // `apply_oidc_env` reads these variables after extraction (D-S-2).
```

### 4. Doc comment of `AuthSettings`

Replace the lines

```rust
/// GitHub and Google accept the split env form (`LLMOBS_AUTH__GITHUB__CLIENT_ID`,
/// `LLMOBS_AUTH__GOOGLE__CLIENT_SECRET`, and the other scalar fields).
/// `[[auth.oidc]]` is TOML only. `LLMOBS_AUTH__OIDC__*` is ignored.
```

with

```rust
/// GitHub and Google accept the split env form (`LLMOBS_AUTH__GITHUB__CLIENT_ID`,
/// `LLMOBS_AUTH__GOOGLE__CLIENT_SECRET`, and the other scalar fields).
/// `[[auth.oidc]]` providers are defined in TOML; only their client secret has an env
/// form, `LLMOBS_AUTH__OIDC__<NAME>__CLIENT_SECRET` (D-S-2).
```

and on the field `oidc`, replace `/// Generic OIDC providers, in config order. TOML only.` with `/// Generic OIDC providers, in config order. TOML, plus each one's client secret from the env.`

### 5. Tests (in `config.rs`'s `mod tests`)

Rename `github_client_secret_stays_a_string_and_oidc_env_is_ignored` to `github_client_secret_stays_a_string`, and delete from it the two parts about OIDC: the pair `("LLMOBS_AUTH__OIDC__0__NAME", "okta")`, `("LLMOBS_AUTH__OIDC__0__CLIENT_SECRET", "from-env")` from the first `load_from`'s env, the line `assert!(config.auth.oidc.is_empty());`, and the whole `let kept = …; assert_eq!(kept.auth.oidc[0]…);` block. The GitHub assertions and the "expected a string" part stay.

Add this constant next to `PROVIDERS_TOML` (a provider without `client_secret`):

```rust
    const OIDC_WITHOUT_SECRET: &str = r#"
[[auth.oidc]]
name = "my-idp"
display_name = "My IdP"
issuer = "https://idp.example"
client_id = "client"
"#;
```

Add these tests:

1. `oidc_client_secret_comes_from_the_env`: `load_from(Some(OIDC_WITHOUT_SECRET), &[("LLMOBS_AUTH__OIDC__MY_IDP__CLIENT_SECRET", "987654321")])` succeeds, and `config.auth.oidc[0].client_secret.expose() == "987654321"` (an all-digit secret stays the same string: it never goes through figment's env parser). `format!("{:?}", config.auth.oidc[0])` does not contain `987654321`.
2. `oidc_env_name_ignores_case`: the same with the variable written `llmobs_auth__oidc__my_idp__client_secret` → the secret is applied.
3. `oidc_env_secret_wins_over_the_file`: `load_from(Some(&oidc("")), &[("LLMOBS_AUTH__OIDC__OKTA__CLIENT_SECRET", "from-env")])` → `client_secret.expose() == "from-env"` (the `oidc` helper's provider is `okta` with `client_secret = "secret"`).
4. `oidc_secret_is_still_required`: `assert_rejected(OIDC_WITHOUT_SECRET, "auth.oidc.my-idp.client_secret", "client secret is required")`; and `load_from(Some(OIDC_WITHOUT_SECRET), &[("LLMOBS_AUTH__OIDC__MY_IDP__CLIENT_SECRET", "")])` fails with the same text.
5. `stray_oidc_env_is_refused`: for each of `LLMOBS_AUTH__OIDC__OTHER__CLIENT_SECRET` (no such provider), `LLMOBS_AUTH__OIDC__0__CLIENT_SECRET`, `LLMOBS_AUTH__OIDC__OKTA__CLIENT_ID` and `LLMOBS_AUTH__OIDC__0__NAME`, with the value `"leaked-value"`, `load_from(Some(&oidc("")), &[(var, "leaked-value")])` fails; its `to_string()` equals `format!("{var}: only LLMOBS_AUTH__OIDC__<NAME>__CLIENT_SECRET is read, for a provider of the config file")` and does not contain `leaked-value`.
6. `oidc_env_without_any_provider_is_refused`: `load_from(None, &[("LLMOBS_AUTH__OIDC__CLOUD__CLIENT_SECRET", "x")])` fails with the same reason (the key is `LLMOBS_AUTH__OIDC__CLOUD__CLIENT_SECRET`).
7. `oidc_env_name_maps_dashes`: `assert_eq!(oidc_env_name("cloud"), "LLMOBS_AUTH__OIDC__CLOUD__CLIENT_SECRET")` and `assert_eq!(oidc_env_name("my-idp-2"), "LLMOBS_AUTH__OIDC__MY_IDP_2__CLIENT_SECRET")`.

`providers_parse_and_debug_hides_client_secrets` and the other existing tests must keep passing unchanged.

### 6. Spec 12

In `docs/spec/12-operations.md` § Configuration, replace

```markdown
GitHub and Google also accept the split form (`LLMOBS_AUTH__GITHUB__CLIENT_ID`, `LLMOBS_AUTH__GOOGLE__CLIENT_SECRET`, and the other scalar fields of those tables). `[[auth.oidc]]` is TOML only; `LLMOBS_AUTH__OIDC__*` is ignored.
```

with

```markdown
GitHub and Google also accept the split form (`LLMOBS_AUTH__GITHUB__CLIENT_ID`, `LLMOBS_AUTH__GOOGLE__CLIENT_SECRET`, and the other scalar fields of those tables). `[[auth.oidc]]` providers are defined in TOML; only their client secret has an env form, `LLMOBS_AUTH__OIDC__<NAME>__CLIENT_SECRET`, with the provider's `name` upper-cased and `-` written `_` (`my-idp` → `LLMOBS_AUTH__OIDC__MY_IDP__CLIENT_SECRET`). It wins over `client_secret` in the file, which may then be left out. Any other `LLMOBS_AUTH__OIDC__*` variable, or one naming no provider of the file, is a load error (D-S-2).
```

### 7. Spec 08

In `docs/spec/08-auth.md` § OAuth / OIDC, in the TOML block, replace the `[[auth.oidc]]` line `client_secret = "…"` (the one after `client_id = "…"` under `issuer = "https://acme.okta.com"`) with:

```toml
client_secret = "…"      # or LLMOBS_AUTH__OIDC__OKTA__CLIENT_SECRET (spec 12, D-S-2)
```

Change only that one line of the block (GitHub and Google keep theirs).

### 8. Self-hosting docs

In `docs/self-hosting/configuration.md` § Secrets, after the first paragraph (ending "Logs never contain secret values."), add a paragraph:

```markdown
An OpenID Connect provider (`[[auth.oidc]]` in `llmobs.toml`) takes its client secret from
`LLMOBS_AUTH__OIDC__<NAME>__CLIENT_SECRET`, with the provider's `name` in upper case and `-`
written `_` (`name = "my-idp"` → `LLMOBS_AUTH__OIDC__MY_IDP__CLIENT_SECRET`). Leave
`client_secret` out of the file then.
```

### 9. CHANGELOG

Under `## [Unreleased]` → `### Added`, add:

```markdown
- Auth: an `[[auth.oidc]]` provider's client secret can come from
  `LLMOBS_AUTH__OIDC__<NAME>__CLIENT_SECRET`, and `client_secret` may then be left out of
  `llmobs.toml` (D-S-2).
```

Under `### Upgrade notes`, add:

```markdown
- An `LLMOBS_AUTH__OIDC__*` environment variable other than
  `LLMOBS_AUTH__OIDC__<NAME>__CLIENT_SECRET` for a provider of the config file now stops the
  server at start; such variables used to be ignored. Remove them.
```

## Check

```bash
cargo test -p llmobs-server config::
just check
```

## Done when

- The seven tests above and every existing config test pass; `just check` is green.
- `grep -n "LLMOBS_AUTH__OIDC__\*\` is ignored" -r docs crates` finds nothing.
- One commit with the subject above.
