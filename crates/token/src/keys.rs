// SPDX-License-Identifier: Elastic-2.0
//! Built-in public keys and the kinds each may sign (D-P-6, D-P-27).

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::VerifyingKey;

use crate::Kind;

/// One trusted public key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltInKey {
    /// Key id, `[a-z0-9-]{1,32}`.
    pub kid: &'static str,
    /// ed25519 public key, standard base64.
    pub public: &'static str,
    /// The token kinds it may sign.
    pub kinds: &'static [Kind],
}

/// Keys every build trusts. `prod-1` signs licences only and stays offline (D-P-27).
/// P11 adds `cloud-1` and `cloud-2` for org and control-plane tokens.
const RELEASE_KEYS: &[BuiltInKey] = &[BuiltInKey {
    kid: "prod-1",
    public: "AvIxTEduiC/amifl8QFxLYjXjPYON8LsreI7sL7UDWE=",
    kinds: &[Kind::License],
}];

/// Keys only debug builds trust. The private half is `fixtures/license/dev-1.key`.
const DEV_KEYS: &[BuiltInKey] = &[BuiltInKey {
    kid: "dev-1",
    public: "kMTjOGaR2ry6zgwZ2p2nk0kMsOoyisgt3qfRm4JH36Y=",
    kinds: &[Kind::License, Kind::Org, Kind::ControlPlane],
}];

/// The keys this build trusts: [`RELEASE_KEYS`], plus [`DEV_KEYS`] when built with debug
/// assertions (evaluated in this crate's profile).
pub fn built_in_keys() -> impl Iterator<Item = &'static BuiltInKey> {
    let dev: &'static [BuiltInKey] = if cfg!(debug_assertions) {
        DEV_KEYS
    } else {
        &[]
    };
    RELEASE_KEYS.iter().chain(dev)
}

/// A standard-base64 ed25519 public key. `None` when it is not 32 valid bytes; the
/// `keys_decode` test proves every built-in key decodes.
pub(crate) fn decode(public: &str) -> Option<VerifyingKey> {
    let bytes = STANDARD.decode(public).ok()?;
    let bytes: [u8; 32] = bytes.try_into().ok()?;
    VerifyingKey::from_bytes(&bytes).ok()
}
