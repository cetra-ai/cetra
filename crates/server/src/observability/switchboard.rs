//! Who may send what to us (D-TL-11). [`decide`] runs once at startup over values captured by the config loader, so tests inject the environment without touching the process. The first matching rule wins; nothing here reads licence state, and licences never change the result (D172).

/// The `tracing` target of the startup notice. The stdout filter always lets it through at
/// `info` ([`NOTICE_DIRECTIVE`]), so the notice shows at `log.level = "warn"` too.
pub(crate) const NOTICE_TARGET: &str = "cetra_server::observability::notice";

/// The filter directive the stdout filter adds last, after the `rmcp` cap.
pub(crate) const NOTICE_DIRECTIVE: &str = "cetra_server::observability::notice=info";

/// Variables whose non-empty value marks a CI environment (besides `CI`, which is read
/// with the `false`/`0` exception).
const CI_VARIABLES: [&str; 13] = [
    "GITHUB_ACTIONS",
    "GITLAB_CI",
    "BUILDKITE",
    "CIRCLECI",
    "JENKINS_URL",
    "TF_BUILD",
    "TRAVIS",
    "TEAMCITY_VERSION",
    "BITBUCKET_BUILD_NUMBER",
    "CODEBUILD_BUILD_ID",
    "DRONE",
    "APPVEYOR",
    "SEMAPHORE",
];

/// What the switchboard reads from the process environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct TelemetryEnv {
    /// `CETRA_TELEMETRY` is `off`, `false` or `0` (any case).
    pub(crate) forced_off: bool,
    /// `DO_NOT_TRACK` is non-empty and not `0` or `false` (any case).
    pub(crate) do_not_track: bool,
    /// `CI` is non-empty and not `false` or `0`, or a [`CI_VARIABLES`] name is non-empty.
    pub(crate) ci: bool,
}

/// A `CETRA_TELEMETRY` value that is not `off`, `false`, `0` or empty. Carries no value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum TelemetryEnvError {
    /// The message names the accepted values, never the value given.
    #[error("CETRA_TELEMETRY: accepted values are off, false and 0 (any case)")]
    InvalidValue,
}

impl TelemetryEnv {
    /// Reads the switchboard's variables from `env`, the pairs the config loader reads.
    ///
    /// `CETRA_TELEMETRY` matches without regard to case, like every `CETRA_` name;
    /// `DO_NOT_TRACK` and the CI names match exactly. Values are trimmed first.
    ///
    /// # Errors
    ///
    /// [`TelemetryEnvError::InvalidValue`] when a `CETRA_TELEMETRY` value is not `off`,
    /// `false`, `0` or empty, so a typo never leaves telemetry on (`on` is refused too).
    pub(crate) fn from_pairs(env: &[(&str, &str)]) -> Result<Self, TelemetryEnvError> {
        let mut found = Self::default();
        for (name, raw) in env {
            let value = raw.trim();
            if name.eq_ignore_ascii_case("CETRA_TELEMETRY") {
                if value.is_empty() {
                    continue;
                }
                if ["off", "false", "0"]
                    .iter()
                    .any(|off| value.eq_ignore_ascii_case(off))
                {
                    found.forced_off = true;
                } else {
                    return Err(TelemetryEnvError::InvalidValue);
                }
            } else if *name == "DO_NOT_TRACK" {
                found.do_not_track |= is_set_and_not_false(value);
            } else if *name == "CI" {
                found.ci |= is_set_and_not_false(value);
            } else if CI_VARIABLES.contains(name) {
                found.ci |= !value.is_empty();
            }
        }
        Ok(found)
    }
}

/// Non-empty and not `0` or `false` (any case).
fn is_set_and_not_false(value: &str) -> bool {
    !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
}

/// Why something that may go to us is off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OffReason {
    /// Rule 1: `CETRA_TELEMETRY=off`.
    CetraTelemetry,
    /// Rule 2: `DO_NOT_TRACK`.
    DoNotTrack,
    /// Rule 3: `restore_mode = true` (D-N-25).
    RestoreMode,
    /// Rule 4, heartbeat only: a build with `debug_assertions`.
    DebugBuild,
    /// Rule 4, heartbeat only: a CI environment.
    Ci,
    /// Rule 5: the config turns it off.
    Config,
}

impl OffReason {
    /// The `reason` field of the startup notice.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::CetraTelemetry => "CETRA_TELEMETRY",
            Self::DoNotTrack => "DO_NOT_TRACK",
            Self::RestoreMode => "restore_mode",
            Self::DebugBuild => "debug_build",
            Self::Ci => "ci",
            Self::Config => "config",
        }
    }
}

/// On, or off with the first rule that turned it off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Switch {
    /// May be sent.
    On,
    /// Must not be sent.
    Off(OffReason),
}

/// What may go to us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Decision {
    /// The daily heartbeat (D-TL-8).
    pub(crate) heartbeat: Switch,
    /// Crash reports (D-TL-5).
    pub(crate) crash_reports: Switch,
}

/// Everything [`decide`] looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Inputs {
    /// The captured environment.
    pub(crate) env: TelemetryEnv,
    /// `restore_mode`.
    pub(crate) restore_mode: bool,
    /// `cfg!(debug_assertions)` in production; tests pass either value.
    pub(crate) debug_build: bool,
    /// `telemetry.heartbeat`.
    pub(crate) heartbeat: bool,
    /// `telemetry.crash_reports`.
    pub(crate) crash_reports: bool,
}

/// The switchboard (D-TL-11). Total: every input gives a decision, so there is no error
/// path that could leave something on.
pub(crate) fn decide(inputs: &Inputs) -> Decision {
    let shared = shared_off(inputs);
    let heartbeat = match shared {
        Some(reason) => Switch::Off(reason),
        None if inputs.debug_build => Switch::Off(OffReason::DebugBuild),
        None if inputs.env.ci => Switch::Off(OffReason::Ci),
        None if !inputs.heartbeat => Switch::Off(OffReason::Config),
        None => Switch::On,
    };
    // Rule 4 (debug builds, CI) is heartbeat only: crash reports run only when asked for.
    let crash_reports = match shared {
        Some(reason) => Switch::Off(reason),
        None if !inputs.crash_reports => Switch::Off(OffReason::Config),
        None => Switch::On,
    };
    Decision {
        heartbeat,
        crash_reports,
    }
}

/// The decision for this process: [`decide`] over `config` and this build.
pub(crate) fn for_config(config: &crate::config::ServerConfig) -> Decision {
    decide(&Inputs {
        env: config.telemetry.env,
        restore_mode: config.restore_mode,
        debug_build: cfg!(debug_assertions),
        heartbeat: config.telemetry.heartbeat,
        crash_reports: config.telemetry.crash_reports,
    })
}

/// Rules 1–3, which turn off everything that goes to us.
fn shared_off(inputs: &Inputs) -> Option<OffReason> {
    if inputs.env.forced_off {
        Some(OffReason::CetraTelemetry)
    } else if inputs.env.do_not_track {
        Some(OffReason::DoNotTrack)
    } else if inputs.restore_mode {
        Some(OffReason::RestoreMode)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clear() -> Inputs {
        Inputs {
            env: TelemetryEnv::default(),
            restore_mode: false,
            debug_build: false,
            heartbeat: true,
            crash_reports: false,
        }
    }

    #[test]
    fn nothing_set_is_the_default() {
        assert_eq!(TelemetryEnv::from_pairs(&[]), Ok(TelemetryEnv::default()));
    }

    #[test]
    fn off_false_and_zero_force_off_in_any_case() {
        for value in ["off", "OFF", "Off", "false", "FALSE", "0", " off "] {
            let env = TelemetryEnv::from_pairs(&[("CETRA_TELEMETRY", value)]);
            assert_eq!(env.map(|e| e.forced_off), Ok(true), "value {value:?}");
        }
    }

    #[test]
    fn an_empty_cetra_telemetry_is_ignored() {
        for value in ["", "  "] {
            let env = TelemetryEnv::from_pairs(&[("CETRA_TELEMETRY", value)]);
            assert_eq!(env.map(|e| e.forced_off), Ok(false), "value {value:?}");
        }
    }

    #[test]
    fn any_other_value_is_an_error() {
        for value in ["on", "true", "1", "no", "disabled", "offf", "of"] {
            let env = TelemetryEnv::from_pairs(&[("CETRA_TELEMETRY", value)]);
            assert!(
                matches!(env, Err(TelemetryEnvError::InvalidValue)),
                "value {value:?}"
            );
        }
    }

    #[test]
    fn the_error_never_carries_the_value() {
        let err = TelemetryEnv::from_pairs(&[("CETRA_TELEMETRY", "planted-value-7f3")])
            .expect_err("an invalid value is an error");
        assert!(!err.to_string().contains("planted-value-7f3"));
    }

    #[test]
    fn the_cetra_telemetry_name_matches_in_any_case() {
        let forced = TelemetryEnv::from_pairs(&[("cetra_telemetry", "off")]);
        assert_eq!(forced.map(|e| e.forced_off), Ok(true));
        let invalid = TelemetryEnv::from_pairs(&[("Cetra_Telemetry", "on")]);
        assert!(matches!(invalid, Err(TelemetryEnvError::InvalidValue)));
    }

    #[test]
    fn do_not_track_values() {
        for value in ["1", "true", "yes", "anything"] {
            let env = TelemetryEnv::from_pairs(&[("DO_NOT_TRACK", value)]);
            assert_eq!(env.map(|e| e.do_not_track), Ok(true), "value {value:?}");
        }
        for value in ["0", "false", "FALSE", ""] {
            let env = TelemetryEnv::from_pairs(&[("DO_NOT_TRACK", value)]);
            assert_eq!(env.map(|e| e.do_not_track), Ok(false), "value {value:?}");
        }
        assert_eq!(
            TelemetryEnv::from_pairs(&[]).map(|e| e.do_not_track),
            Ok(false)
        );
    }

    #[test]
    fn do_not_track_name_is_exact() {
        let env = TelemetryEnv::from_pairs(&[("do_not_track", "1")]);
        assert_eq!(env.map(|e| e.do_not_track), Ok(false));
    }

    #[test]
    fn ci_values() {
        for value in ["true", "1"] {
            let env = TelemetryEnv::from_pairs(&[("CI", value)]);
            assert_eq!(env.map(|e| e.ci), Ok(true), "value {value:?}");
        }
        for value in ["false", "0", ""] {
            let env = TelemetryEnv::from_pairs(&[("CI", value)]);
            assert_eq!(env.map(|e| e.ci), Ok(false), "value {value:?}");
        }
    }

    #[test]
    fn every_ci_variable_marks_ci() {
        for name in CI_VARIABLES {
            let set = TelemetryEnv::from_pairs(&[(name, "x")]);
            assert_eq!(set.map(|e| e.ci), Ok(true), "name {name}");
            let empty = TelemetryEnv::from_pairs(&[(name, "")]);
            assert_eq!(empty.map(|e| e.ci), Ok(false), "name {name}");
        }
    }

    #[test]
    fn unrelated_variables_are_ignored() {
        let env = TelemetryEnv::from_pairs(&[
            ("HOME", "/root"),
            ("CETRA_LOG", "debug"),
            ("CETRA_TELEMETRY__HEARTBEAT", "false"),
        ]);
        assert_eq!(env, Ok(TelemetryEnv::default()));
    }

    #[test]
    fn all_clear_is_on() {
        assert_eq!(decide(&clear()).heartbeat, Switch::On);
    }

    #[test]
    fn cetra_telemetry_is_rule_one() {
        let inputs = Inputs {
            env: TelemetryEnv {
                forced_off: true,
                do_not_track: true,
                ci: true,
            },
            restore_mode: true,
            debug_build: true,
            heartbeat: true,
            crash_reports: false,
        };
        assert_eq!(
            decide(&inputs).heartbeat,
            Switch::Off(OffReason::CetraTelemetry)
        );
    }

    #[test]
    fn do_not_track_is_rule_two() {
        let inputs = Inputs {
            env: TelemetryEnv {
                forced_off: false,
                do_not_track: true,
                ci: false,
            },
            restore_mode: true,
            debug_build: true,
            heartbeat: true,
            crash_reports: false,
        };
        assert_eq!(
            decide(&inputs).heartbeat,
            Switch::Off(OffReason::DoNotTrack)
        );
    }

    #[test]
    fn restore_mode_is_rule_three() {
        let inputs = Inputs {
            env: TelemetryEnv {
                forced_off: false,
                do_not_track: false,
                ci: true,
            },
            restore_mode: true,
            debug_build: true,
            heartbeat: true,
            crash_reports: false,
        };
        assert_eq!(
            decide(&inputs).heartbeat,
            Switch::Off(OffReason::RestoreMode)
        );
    }

    #[test]
    fn a_debug_build_is_off() {
        let inputs = Inputs {
            env: TelemetryEnv {
                forced_off: false,
                do_not_track: false,
                ci: true,
            },
            debug_build: true,
            ..clear()
        };
        assert_eq!(
            decide(&inputs).heartbeat,
            Switch::Off(OffReason::DebugBuild)
        );
    }

    #[test]
    fn ci_is_off() {
        let inputs = Inputs {
            env: TelemetryEnv {
                forced_off: false,
                do_not_track: false,
                ci: true,
            },
            ..clear()
        };
        assert_eq!(decide(&inputs).heartbeat, Switch::Off(OffReason::Ci));
    }

    #[test]
    fn the_config_is_rule_five() {
        let inputs = Inputs {
            heartbeat: false,
            ..clear()
        };
        assert_eq!(decide(&inputs).heartbeat, Switch::Off(OffReason::Config));
    }

    #[test]
    fn the_config_cannot_turn_on_what_a_rule_turned_off() {
        let rules = [
            Inputs {
                env: TelemetryEnv {
                    forced_off: true,
                    ..TelemetryEnv::default()
                },
                ..clear()
            },
            Inputs {
                env: TelemetryEnv {
                    do_not_track: true,
                    ..TelemetryEnv::default()
                },
                ..clear()
            },
            Inputs {
                restore_mode: true,
                ..clear()
            },
            Inputs {
                debug_build: true,
                ..clear()
            },
            Inputs {
                env: TelemetryEnv {
                    ci: true,
                    ..TelemetryEnv::default()
                },
                ..clear()
            },
        ];
        for inputs in rules {
            assert!(
                matches!(decide(&inputs).heartbeat, Switch::Off(_)),
                "inputs {inputs:?}"
            );
        }
    }

    #[test]
    fn reasons_have_fixed_names() {
        assert_eq!(OffReason::CetraTelemetry.as_str(), "CETRA_TELEMETRY");
        assert_eq!(OffReason::DoNotTrack.as_str(), "DO_NOT_TRACK");
        assert_eq!(OffReason::RestoreMode.as_str(), "restore_mode");
        assert_eq!(OffReason::DebugBuild.as_str(), "debug_build");
        assert_eq!(OffReason::Ci.as_str(), "ci");
        assert_eq!(OffReason::Config.as_str(), "config");
    }

    #[test]
    fn for_config_reads_the_config() {
        let dir = std::path::Path::new("unused-by-config");
        let mut config = crate::test_support::config(dir);
        config.restore_mode = true;
        assert_eq!(
            for_config(&config).heartbeat,
            Switch::Off(OffReason::RestoreMode)
        );
        config.restore_mode = false;
        assert_eq!(
            for_config(&config).heartbeat,
            Switch::Off(OffReason::DebugBuild)
        );
        config.telemetry.env.forced_off = true;
        assert_eq!(
            for_config(&config).heartbeat,
            Switch::Off(OffReason::CetraTelemetry)
        );
    }

    #[test]
    fn crash_reports_are_off_by_default() {
        assert_eq!(
            decide(&clear()).crash_reports,
            Switch::Off(OffReason::Config)
        );
    }

    #[test]
    fn crash_reports_on_when_asked() {
        let inputs = Inputs {
            crash_reports: true,
            ..clear()
        };
        assert_eq!(decide(&inputs).crash_reports, Switch::On);
    }

    #[test]
    fn crash_reports_follow_rules_one_to_three() {
        let asked = Inputs {
            crash_reports: true,
            ..clear()
        };
        let forced = Inputs {
            env: TelemetryEnv {
                forced_off: true,
                ..TelemetryEnv::default()
            },
            ..asked
        };
        assert_eq!(
            decide(&forced).crash_reports,
            Switch::Off(OffReason::CetraTelemetry)
        );
        let do_not_track = Inputs {
            env: TelemetryEnv {
                do_not_track: true,
                ..TelemetryEnv::default()
            },
            ..asked
        };
        assert_eq!(
            decide(&do_not_track).crash_reports,
            Switch::Off(OffReason::DoNotTrack)
        );
        let restored = Inputs {
            restore_mode: true,
            ..asked
        };
        assert_eq!(
            decide(&restored).crash_reports,
            Switch::Off(OffReason::RestoreMode)
        );
        let both = Inputs {
            restore_mode: true,
            env: TelemetryEnv {
                forced_off: true,
                ..TelemetryEnv::default()
            },
            ..asked
        };
        assert_eq!(
            decide(&both).crash_reports,
            Switch::Off(OffReason::CetraTelemetry)
        );
    }

    #[test]
    fn crash_reports_ignore_debug_builds_and_ci() {
        let inputs = Inputs {
            env: TelemetryEnv {
                ci: true,
                ..TelemetryEnv::default()
            },
            debug_build: true,
            crash_reports: true,
            ..clear()
        };
        let decision = decide(&inputs);
        assert_eq!(decision.crash_reports, Switch::On);
        assert_eq!(decision.heartbeat, Switch::Off(OffReason::DebugBuild));
    }

    #[test]
    fn the_two_switches_decide_independently() {
        let heartbeat_off = Inputs {
            heartbeat: false,
            crash_reports: true,
            ..clear()
        };
        let decision = decide(&heartbeat_off);
        assert_eq!(decision.heartbeat, Switch::Off(OffReason::Config));
        assert_eq!(decision.crash_reports, Switch::On);
        let crash_off = Inputs {
            heartbeat: true,
            crash_reports: false,
            ..clear()
        };
        let decision = decide(&crash_off);
        assert_eq!(decision.heartbeat, Switch::On);
        assert_eq!(decision.crash_reports, Switch::Off(OffReason::Config));
    }

    #[test]
    fn for_config_reads_crash_reports() {
        let dir = std::path::Path::new("unused-by-config");
        let mut config = crate::test_support::config(dir);
        config.telemetry.crash_reports = true;
        assert_eq!(for_config(&config).crash_reports, Switch::On);
        config.restore_mode = true;
        assert_eq!(
            for_config(&config).crash_reports,
            Switch::Off(OffReason::RestoreMode)
        );
    }

    #[test]
    fn the_notice_directive_names_the_notice_target() {
        assert_eq!(NOTICE_DIRECTIVE, format!("{NOTICE_TARGET}=info"));
    }
}
