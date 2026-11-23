// SPDX-License-Identifier: Elastic-2.0
//! SCIM PATCH bodies for users, normalized (RFC 7644 § 3.5.2, track EA M5, D-EA-12). Okta and
//! Entra ID send `active` as a boolean or as a string, with a path or as a bare object; all those
//! forms are read here. An understood operation on an attribute cetra does not store is a no-op.

use serde_json::Value;

use super::error::ScimError;
use super::filter::strip_prefix_ci;

pub(crate) const PATCH_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:PatchOp";
pub(crate) const ENTERPRISE_USER: &str =
    "urn:ietf:params:scim:schemas:extension:enterprise:2.0:User";

/// The most operations one PATCH may carry.
const MAX_OPERATIONS: usize = 100;

/// The Entra path for the work email, lowercased as [`attribute_of`] returns it.
const WORK_EMAIL_PATH: &str = "emails[type eq \"work\"].value";

/// One normalized change to a SCIM user. Its values are names and emails: it is never logged.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum UserChange {
    Active(bool),
    UserName(String),
    ExternalId(Option<String>),
    GivenName(Option<String>),
    FamilyName(Option<String>),
    DisplayName(Option<String>),
    Department(Option<String>),
    EmployeeNumber(Option<String>),
    /// A `work` or primary email; equals the userName in cetra.
    Email(String),
}

/// The changes a PATCH body asks for, in order. Unsupported attributes give no change.
///
/// # Errors
///
/// Returns `invalidSyntax` for a body without the PATCH schema, without a non-empty operations
/// array, or with an operation that is not `add`, `replace` or `remove`. Returns `invalidValue`
/// for more than 100 operations, a `remove` of `active` or `userName`, and a value of the wrong
/// type.
pub(crate) fn user_changes(body: &Value) -> Result<Vec<UserChange>, ScimError> {
    let schemas = body
        .get("schemas")
        .and_then(Value::as_array)
        .ok_or_else(ScimError::invalid_syntax)?;
    if !schemas
        .iter()
        .any(|schema| schema.as_str() == Some(PATCH_SCHEMA))
    {
        return Err(ScimError::invalid_syntax());
    }
    let operations = body
        .get("Operations")
        .or_else(|| body.get("operations"))
        .and_then(Value::as_array)
        .ok_or_else(ScimError::invalid_syntax)?;
    if operations.is_empty() {
        return Err(ScimError::invalid_syntax());
    }
    if operations.len() > MAX_OPERATIONS {
        return Err(ScimError::invalid_value(
            "At most 100 operations are accepted.",
        ));
    }
    let mut changes = Vec::new();
    for operation in operations {
        changes.extend(operation_changes(operation)?);
    }
    Ok(changes)
}

fn operation_changes(operation: &Value) -> Result<Vec<UserChange>, ScimError> {
    let fields = operation
        .as_object()
        .ok_or_else(ScimError::invalid_syntax)?;
    let op = fields
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(ScimError::invalid_syntax)?;
    let path = match fields.get("path") {
        None => None,
        Some(Value::String(path)) => Some(path.as_str()),
        Some(_) => return Err(ScimError::invalid_syntax()),
    };
    if op.eq_ignore_ascii_case("remove") {
        let path = path.ok_or_else(ScimError::invalid_syntax)?;
        return Ok(remove_change(path)?.into_iter().collect());
    }
    if !(op.eq_ignore_ascii_case("add") || op.eq_ignore_ascii_case("replace")) {
        return Err(ScimError::invalid_syntax());
    }
    let value = fields.get("value").ok_or_else(ScimError::invalid_syntax)?;
    match path {
        Some(path) => Ok(path_change(path, value)?.into_iter().collect()),
        None => map_changes(value),
    }
}

/// The attribute a path names, lowercased, with the enterprise URN prefix removed.
fn attribute_of(path: &str) -> String {
    let attribute = strip_prefix_ci(path, &format!("{ENTERPRISE_USER}:")).unwrap_or(path);
    attribute.to_ascii_lowercase()
}

/// The change of a `remove` with a path.
fn remove_change(path: &str) -> Result<Option<UserChange>, ScimError> {
    let change = match attribute_of(path).as_str() {
        "active" => return Err(ScimError::invalid_value("active cannot be removed.")),
        "username" | "emails.value" | WORK_EMAIL_PATH => {
            return Err(ScimError::invalid_value("userName cannot be removed."));
        }
        "externalid" => UserChange::ExternalId(None),
        "displayname" => UserChange::DisplayName(None),
        "name.givenname" => UserChange::GivenName(None),
        "name.familyname" => UserChange::FamilyName(None),
        "department" => UserChange::Department(None),
        "employeenumber" => UserChange::EmployeeNumber(None),
        _ => return Ok(None),
    };
    Ok(Some(change))
}

/// The change of an `add` or `replace` with a path and a value.
fn path_change(path: &str, value: &Value) -> Result<Option<UserChange>, ScimError> {
    let change = match attribute_of(path).as_str() {
        "active" => UserChange::Active(boolean(value)?),
        "username" => UserChange::UserName(required_string(value)?),
        "emails.value" | WORK_EMAIL_PATH => UserChange::Email(required_string(value)?),
        "externalid" => UserChange::ExternalId(optional_string(value)?),
        "displayname" => UserChange::DisplayName(optional_string(value)?),
        "name.givenname" => UserChange::GivenName(optional_string(value)?),
        "name.familyname" => UserChange::FamilyName(optional_string(value)?),
        "department" => UserChange::Department(optional_string(value)?),
        "employeenumber" => UserChange::EmployeeNumber(optional_string(value)?),
        _ => return Ok(None),
    };
    Ok(Some(change))
}

/// The changes of an `add` or `replace` without a path. Each key is handled as a path; `name`
/// and the enterprise URN hold objects of their own.
pub(crate) fn map_changes(value: &Value) -> Result<Vec<UserChange>, ScimError> {
    let fields = value.as_object().ok_or_else(ScimError::invalid_syntax)?;
    let mut changes = Vec::new();
    for (key, item) in fields {
        if key.eq_ignore_ascii_case(ENTERPRISE_USER) {
            let extension = item.as_object().ok_or_else(|| {
                ScimError::invalid_value("The enterprise extension must be an object.")
            })?;
            for (field, field_value) in extension {
                changes.extend(path_change(field, field_value)?);
            }
        } else if key.eq_ignore_ascii_case("name") {
            let name = item
                .as_object()
                .ok_or_else(|| ScimError::invalid_value("name must be an object."))?;
            for (field, field_value) in name {
                changes.extend(path_change(&format!("name.{field}"), field_value)?);
            }
        } else if key.eq_ignore_ascii_case("emails") {
            changes.extend(emails_changes(item)?);
        } else {
            changes.extend(path_change(key, item)?);
        }
    }
    Ok(changes)
}

/// The email of an `emails` array: the `work` entry, else the `primary` one, else the first.
fn emails_changes(value: &Value) -> Result<Vec<UserChange>, ScimError> {
    let emails = value
        .as_array()
        .ok_or_else(|| ScimError::invalid_value("emails must be an array."))?;
    if emails.iter().any(|email| !email.is_object()) {
        return Err(ScimError::invalid_value("emails must be objects."));
    }
    let chosen = emails
        .iter()
        .find(|email| email.get("type").and_then(Value::as_str) == Some("work"))
        .or_else(|| {
            emails
                .iter()
                .find(|email| email.get("primary").and_then(Value::as_bool) == Some(true))
        })
        .or_else(|| emails.first());
    let Some(email) = chosen else {
        return Ok(Vec::new());
    };
    let address = email
        .get("value")
        .and_then(Value::as_str)
        .ok_or_else(|| ScimError::invalid_value("userName must be a string."))?;
    Ok(vec![UserChange::Email(address.to_owned())])
}

/// `active`: a boolean, or the string `true` or `false` in either common capitalisation.
fn boolean(value: &Value) -> Result<bool, ScimError> {
    match value {
        Value::Bool(flag) => Ok(*flag),
        Value::String(text) if matches!(text.as_str(), "true" | "True") => Ok(true),
        Value::String(text) if matches!(text.as_str(), "false" | "False") => Ok(false),
        _ => Err(ScimError::invalid_value("active must be a boolean")),
    }
}

/// The userName, or an email path's value: a string.
fn required_string(value: &Value) -> Result<String, ScimError> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| ScimError::invalid_value("userName must be a string."))
}

/// An optional attribute: a string, or `null` to clear it.
fn optional_string(value: &Value) -> Result<Option<String>, ScimError> {
    match value {
        Value::Null => Ok(None),
        Value::String(text) => Ok(Some(text.clone())),
        _ => Err(ScimError::invalid_value(
            "Attribute values must be strings or null.",
        )),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// A PATCH body with the given operations.
    #[allow(clippy::needless_pass_by_value)] // callers build the value inline
    fn body(operations: Value) -> Value {
        json!({"schemas": [PATCH_SCHEMA], "Operations": operations})
    }

    #[test]
    fn okta_deactivate() {
        let changes = user_changes(&body(json!([
            {"op": "replace", "value": {"active": false}}
        ])))
        .unwrap();
        assert_eq!(changes, vec![UserChange::Active(false)]);
    }

    #[test]
    fn entra_deactivate_with_string_false() {
        let changes = user_changes(&body(json!([
            {"op": "Replace", "path": "active", "value": "False"}
        ])))
        .unwrap();
        assert_eq!(changes, vec![UserChange::Active(false)]);
    }

    #[test]
    fn entra_enterprise_department_path() {
        let changes = user_changes(&body(json!([
            {
                "op": "add",
                "path": "urn:ietf:params:scim:schemas:extension:enterprise:2.0:User:department",
                "value": "Engineering"
            }
        ])))
        .unwrap();
        assert_eq!(
            changes,
            vec![UserChange::Department(Some("Engineering".to_owned()))]
        );
    }

    #[test]
    fn name_object_and_dotted_keys() {
        let changes = user_changes(&body(json!([
            {"op": "replace", "value": {"name": {"givenName": "Ada"}, "name.familyName": "Lovelace"}}
        ])))
        .unwrap();
        assert_eq!(
            changes,
            vec![
                UserChange::GivenName(Some("Ada".to_owned())),
                UserChange::FamilyName(Some("Lovelace".to_owned())),
            ]
        );
    }

    #[test]
    fn emails_work_filter_path() {
        let changes = user_changes(&body(json!([
            {"op": "replace", "path": "emails[type eq \"work\"].value", "value": "ada@example.com"}
        ])))
        .unwrap();
        assert_eq!(
            changes,
            vec![UserChange::Email("ada@example.com".to_owned())]
        );
    }

    #[test]
    fn emails_array_picks_the_work_entry() {
        let changes = user_changes(&body(json!([
            {"op": "replace", "value": {"emails": [
                {"value": "home@example.com", "type": "home", "primary": true},
                {"value": "work@example.com", "type": "work"}
            ]}}
        ])))
        .unwrap();
        assert_eq!(
            changes,
            vec![UserChange::Email("work@example.com".to_owned())]
        );
    }

    #[test]
    fn unsupported_attribute_is_a_noop() {
        let changes = user_changes(&body(json!([
            {"op": "replace", "path": "title", "value": "Engineer"},
            {"op": "add", "path": "phoneNumbers", "value": []}
        ])))
        .unwrap();
        assert!(changes.is_empty());
    }

    #[test]
    fn missing_schema_is_invalid_syntax() {
        let body = json!({"Operations": [{"op": "replace", "value": {"active": false}}]});
        let error = user_changes(&body).unwrap_err();
        assert_eq!(error.scim_type, Some("invalidSyntax"));
    }

    #[test]
    fn remove_active_is_invalid() {
        let result = user_changes(&body(json!([
            {"op": "remove", "path": "active"}
        ])));
        assert!(result.is_err());
    }

    #[test]
    fn remove_user_name_is_invalid() {
        let result = user_changes(&body(json!([
            {"op": "remove", "path": "userName"}
        ])));
        assert!(result.is_err());
    }

    #[test]
    fn remove_clears_an_optional_attribute() {
        let changes = user_changes(&body(json!([
            {"op": "remove", "path": "displayName"}
        ])))
        .unwrap();
        assert_eq!(changes, vec![UserChange::DisplayName(None)]);
    }

    #[test]
    fn more_than_100_ops_is_invalid() {
        let operations: Vec<Value> = (0..101)
            .map(|_| json!({"op": "replace", "value": {"active": true}}))
            .collect();
        assert!(user_changes(&body(Value::Array(operations))).is_err());
        let hundred: Vec<Value> = (0..100)
            .map(|_| json!({"op": "replace", "value": {"active": true}}))
            .collect();
        assert_eq!(
            user_changes(&body(Value::Array(hundred))).unwrap().len(),
            100
        );
    }

    #[test]
    fn empty_operations_are_invalid() {
        assert!(user_changes(&body(json!([]))).is_err());
    }

    #[test]
    fn capitalised_operations_key() {
        let body = json!({
            "schemas": [PATCH_SCHEMA],
            "operations": [{"op": "REPLACE", "path": "active", "value": true}]
        });
        assert_eq!(user_changes(&body).unwrap(), vec![UserChange::Active(true)]);
    }

    #[test]
    fn active_accepts_the_four_string_forms() {
        for (text, expected) in [
            ("true", true),
            ("True", true),
            ("false", false),
            ("False", false),
        ] {
            let changes = user_changes(&body(json!([
                {"op": "replace", "path": "active", "value": text}
            ])))
            .unwrap();
            assert_eq!(changes, vec![UserChange::Active(expected)]);
        }
        assert!(
            user_changes(&body(json!([
                {"op": "replace", "path": "active", "value": "yes"}
            ])))
            .is_err()
        );
    }

    #[test]
    fn null_clears_an_optional_string() {
        let changes = user_changes(&body(json!([
            {"op": "replace", "path": "externalId", "value": null}
        ])))
        .unwrap();
        assert_eq!(changes, vec![UserChange::ExternalId(None)]);
    }
}
