//! Online rule API types (spec 09 § Evaluation & alerts, D-I-15, D-I-30).

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The period of a rule budget, in UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BudgetPeriod {
    /// The budget applies to each UTC day.
    Day,
    /// The budget applies to each UTC calendar month.
    Month,
}

impl BudgetPeriod {
    /// The stored budget period.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Month => "month",
        }
    }

    /// Parses the stored budget period. Any other text is `None`.
    pub fn from_name(text: &str) -> Option<Self> {
        match text {
            "day" => Some(Self::Day),
            "month" => Some(Self::Month),
            _ => None,
        }
    }
}
