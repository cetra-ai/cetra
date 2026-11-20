//! How good a score value is, and whether it is bad enough to make a candidate (D-I-47, D-IC-4).
//! A port of `web/src/lib/score-tone.ts`: keep the branches in step with it.

use cetra_core::score::ScoreValue;

use crate::api::score_definitions::ScoreDirection;
use crate::meta::score_definitions::DefinitionRow;

/// Where `value` sits between bad (0) and good (1) under `definition`, or `None` when it is
/// neutral (D-I-47). Without a live definition a boolean is 1 when true and 0 when false; every
/// other value is neutral.
pub(crate) fn goodness(value: &ScoreValue, definition: Option<&DefinitionRow>) -> Option<f64> {
    let Some(definition) = definition.filter(|def| def.archived_at.is_none()) else {
        return match value {
            ScoreValue::Boolean(true) => Some(1.0),
            ScoreValue::Boolean(false) => Some(0.0),
            ScoreValue::Numeric(_) | ScoreValue::Categorical { .. } | ScoreValue::Text(_) => None,
        };
    };
    if definition.direction == ScoreDirection::None || value.data_type() != definition.data_type {
        return None;
    }
    match value {
        ScoreValue::Numeric(number) => {
            let (min, max) = (definition.min_value?, definition.max_value?);
            oriented(position(*number, min, max)?, definition.direction)
        }
        ScoreValue::Boolean(flag) => oriented(if *flag { 1.0 } else { 0.0 }, definition.direction),
        ScoreValue::Categorical { label, .. } => {
            let category = definition
                .categories
                .iter()
                .find(|cat| cat.label == *label)?;
            let own = category.value?;
            // `own` is one of the values, so the fold starts from a non-empty list.
            let values = definition.categories.iter().filter_map(|cat| cat.value);
            let min = values.clone().fold(f64::INFINITY, f64::min);
            let max = values.fold(f64::NEG_INFINITY, f64::max);
            oriented(position(own, min, max)?, definition.direction)
        }
        ScoreValue::Text(_) => None,
    }
}

/// A score that makes a candidate (D-IC-4): a `false`, or a goodness of at most a third (D-I-47).
/// An archived definition counts as none. A neutral score is never bad by its value.
pub(crate) fn is_bad(value: &ScoreValue, definition: Option<&DefinitionRow>) -> bool {
    if matches!(value, ScoreValue::Boolean(false)) {
        return true;
    }
    goodness(value, definition).is_some_and(|good| good <= 1.0 / 3.0)
}

/// Position of `value` in `[min, max]`, clamped to `[0, 1]`. `None` when the range is empty.
fn position(value: f64, min: f64, max: f64) -> Option<f64> {
    if max.partial_cmp(&min) != Some(std::cmp::Ordering::Greater) {
        return None;
    }
    Some(((value - min) / (max - min)).clamp(0.0, 1.0))
}

/// Flips a goodness for `lower`, where a smaller value is better. `None` for `none`.
fn oriented(goodness: f64, direction: ScoreDirection) -> Option<f64> {
    match direction {
        ScoreDirection::Higher => Some(goodness),
        ScoreDirection::Lower => Some(1.0 - goodness),
        ScoreDirection::None => None,
    }
}

#[cfg(test)]
mod tests {
    use cetra_core::enums::ScoreDataType;
    use cetra_core::ids::ProjectId;
    use cetra_core::score::ScoreValue;
    use uuid::Uuid;

    use super::{goodness, is_bad};
    use crate::api::score_definitions::ScoreDirection;
    use crate::meta::score_definitions::{DefinitionCategory, DefinitionRow};

    fn definition(
        data_type: ScoreDataType,
        direction: ScoreDirection,
        range: Option<(f64, f64)>,
        categories: &[(&str, Option<f64>)],
    ) -> DefinitionRow {
        DefinitionRow {
            id: Uuid::from_u128(1),
            project_id: ProjectId::from_uuid(Uuid::from_u128(2)),
            name: "quality".to_owned(),
            data_type,
            categories: categories
                .iter()
                .map(|(label, value)| DefinitionCategory {
                    label: (*label).to_owned(),
                    value: *value,
                })
                .collect(),
            min_value: range.map(|(min, _)| min),
            max_value: range.map(|(_, max)| max),
            description: None,
            direction,
            archived_at: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn numeric(direction: ScoreDirection) -> DefinitionRow {
        definition(ScoreDataType::Numeric, direction, Some((0.0, 10.0)), &[])
    }

    fn categorical(direction: ScoreDirection) -> DefinitionRow {
        definition(
            ScoreDataType::Categorical,
            direction,
            None,
            &[("bad", Some(0.0)), ("mid", Some(0.5)), ("good", Some(1.0))],
        )
    }

    fn label(name: &str) -> ScoreValue {
        ScoreValue::Categorical {
            label: name.to_owned(),
            value: None,
        }
    }

    #[test]
    fn numeric_scores_are_placed_in_their_range() {
        let higher = numeric(ScoreDirection::Higher);
        assert_eq!(
            goodness(&ScoreValue::Numeric(2.0), Some(&higher)),
            Some(0.2)
        );
        assert!(is_bad(&ScoreValue::Numeric(3.3), Some(&higher)));
        assert!(!is_bad(&ScoreValue::Numeric(3.4), Some(&higher)));
        // Out-of-range values clamp to the ends.
        assert_eq!(
            goodness(&ScoreValue::Numeric(-5.0), Some(&higher)),
            Some(0.0)
        );
        assert_eq!(
            goodness(&ScoreValue::Numeric(50.0), Some(&higher)),
            Some(1.0)
        );

        let lower = numeric(ScoreDirection::Lower);
        assert_eq!(
            goodness(&ScoreValue::Numeric(9.0), Some(&lower)).map(round),
            Some(0.1)
        );
        assert!(is_bad(&ScoreValue::Numeric(9.0), Some(&lower)));
        assert!(!is_bad(&ScoreValue::Numeric(1.0), Some(&lower)));
    }

    #[test]
    fn numeric_without_a_range_or_direction_is_neutral() {
        let no_range = definition(ScoreDataType::Numeric, ScoreDirection::Higher, None, &[]);
        assert_eq!(goodness(&ScoreValue::Numeric(1.0), Some(&no_range)), None);
        assert!(!is_bad(&ScoreValue::Numeric(1.0), Some(&no_range)));
        let neutral = numeric(ScoreDirection::None);
        assert_eq!(goodness(&ScoreValue::Numeric(1.0), Some(&neutral)), None);
    }

    #[test]
    fn categorical_labels_are_placed_by_their_values() {
        let higher = categorical(ScoreDirection::Higher);
        assert!(is_bad(&label("bad"), Some(&higher)));
        assert!(!is_bad(&label("mid"), Some(&higher)));
        assert!(!is_bad(&label("good"), Some(&higher)));
        let lower = categorical(ScoreDirection::Lower);
        assert!(is_bad(&label("good"), Some(&lower)));
        assert!(!is_bad(&label("bad"), Some(&lower)));
        // An unknown label is neutral.
        assert_eq!(goodness(&label("odd"), Some(&higher)), None);
    }

    #[test]
    fn a_single_distinct_category_value_is_neutral() {
        let one = definition(
            ScoreDataType::Categorical,
            ScoreDirection::Higher,
            None,
            &[("only", Some(1.0)), ("also", Some(1.0))],
        );
        assert_eq!(goodness(&label("only"), Some(&one)), None);
    }

    #[test]
    fn booleans_without_a_definition_are_good_or_bad() {
        assert_eq!(goodness(&ScoreValue::Boolean(true), None), Some(1.0));
        assert_eq!(goodness(&ScoreValue::Boolean(false), None), Some(0.0));
        assert!(is_bad(&ScoreValue::Boolean(false), None));
        assert!(!is_bad(&ScoreValue::Boolean(true), None));
    }

    #[test]
    fn a_boolean_definition_follows_its_direction() {
        let higher = definition(ScoreDataType::Boolean, ScoreDirection::Higher, None, &[]);
        assert!(!is_bad(&ScoreValue::Boolean(true), Some(&higher)));
        assert!(is_bad(&ScoreValue::Boolean(false), Some(&higher)));
        let lower = definition(ScoreDataType::Boolean, ScoreDirection::Lower, None, &[]);
        assert_eq!(
            goodness(&ScoreValue::Boolean(true), Some(&lower)),
            Some(0.0)
        );
        let neutral = definition(ScoreDataType::Boolean, ScoreDirection::None, None, &[]);
        assert_eq!(goodness(&ScoreValue::Boolean(true), Some(&neutral)), None);
    }

    #[test]
    fn text_is_never_placed() {
        let text = definition(ScoreDataType::Text, ScoreDirection::Higher, None, &[]);
        assert_eq!(
            goodness(&ScoreValue::Text("bad".to_owned()), Some(&text)),
            None
        );
        assert!(!is_bad(&ScoreValue::Text("bad".to_owned()), Some(&text)));
    }

    #[test]
    fn a_value_of_another_type_than_the_definition_is_neutral() {
        let higher = numeric(ScoreDirection::Higher);
        assert_eq!(goodness(&ScoreValue::Boolean(true), Some(&higher)), None);
    }

    #[test]
    fn an_archived_definition_counts_as_none() {
        let mut archived = numeric(ScoreDirection::Higher);
        archived.archived_at = Some(1);
        assert_eq!(goodness(&ScoreValue::Numeric(1.0), Some(&archived)), None);
        assert_eq!(
            goodness(&ScoreValue::Boolean(true), Some(&archived)),
            Some(1.0)
        );
    }

    fn round(value: f64) -> f64 {
        (value * 1e9).round() / 1e9
    }
}
