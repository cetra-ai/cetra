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

/// Keys every build trusts. `license-offline-1`, `license-1` and `license-2` sign licences only
/// (D-P-27, D-P-73). `cloud-1` and `cloud-2` sign org and control-plane tokens and never
/// licences.
const RELEASE_KEYS: &[BuiltInKey] = &[
    // Offline, kept by the operator (D-P-6).
    BuiltInKey {
        kid: "license-offline-1",
        public: "AvIxTEduiC/amifl8QFxLYjXjPYON8LsreI7sL7UDWE=",
        kinds: &[Kind::License],
    },
    // Online: the control plane signs licence keys with it as they are sold (D-P-73).
    BuiltInKey {
        kid: "license-1",
        public: "YpuqTbhJ70yENEDjby5g2s2kIVqiAWpm5ja6yk3fs5k=",
        kinds: &[Kind::License],
    },
    // Offline spare (D-P-73): the control plane switches to it if `license-1` leaks; a later
    // release then removes `license-1`.
    BuiltInKey {
        kid: "license-2",
        public: "OLLaZqmdRMbinKTR5MmZfB5r6K+vSgNNstOU8Ccrdec=",
        kinds: &[Kind::License],
    },
    BuiltInKey {
        kid: "cloud-1",
        public: "9Dd2255sYAaeYOUWa2tJOlDMFCDXBT0csdSVVrU4zYY=",
        kinds: &[Kind::Org, Kind::ControlPlane],
    },
    // Offline spare (D-P-27): the control plane switches to it if `cloud-1` leaks; the next
    // release then removes `cloud-1`.
    BuiltInKey {
        kid: "cloud-2",
        public: "hhDDxpReDC1t83KURe1Yp09DXXmupihoMUm7YYqKWZY=",
        kinds: &[Kind::Org, Kind::ControlPlane],
    },
];

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
