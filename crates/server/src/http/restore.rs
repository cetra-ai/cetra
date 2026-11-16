//! Restore mode's answer for user-triggered outbound calls (N M3, D-N-26).

use crate::http::AppState;
use crate::http::error::ApiError;

/// `Err(ApiError::RestoreMode)` (503 `restore_mode`) when the server runs in restore mode.
pub(crate) fn refuse_outbound(state: &AppState, what: &'static str) -> Result<(), ApiError> {
    if state.config.restore_mode {
        return Err(ApiError::RestoreMode { what });
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::api::common::ProblemCode;
    use crate::test_support::{TestApp, TestResponse};

    /// Asserts a 503 `restore_mode` problem whose detail names `what`.
    pub(crate) fn assert_refused(response: &TestResponse, what: &str) {
        assert_eq!(
            response.status,
            503,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        let problem = response.problem();
        assert_eq!(problem.code, ProblemCode::RestoreMode);
        assert_eq!(
            problem.detail.as_deref(),
            Some(format!("{what} is off: this server runs in restore mode").as_str())
        );
    }

    #[tokio::test]
    async fn refuse_outbound_passes_without_restore_mode() {
        let app = TestApp::with_config(|_| {}).await;
        assert!(refuse_outbound(&app.state, "channel test").is_ok());
    }
}
