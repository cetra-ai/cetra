// SPDX-License-Identifier: Elastic-2.0
//! SCIM resource representations and discovery documents (RFC 7643 §§ 4.1, 5, 6, 7; RFC 7644
//! § 3.4.2, track EA M5, D-EA-12). Absent optional values are omitted, never `null`.

use serde_json::{Map, Value, json};

use super::patch::ENTERPRISE_USER;
use super::store::ScimUserRow;

pub(crate) const USER_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
pub(crate) const LIST_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:ListResponse";
const SERVICE_PROVIDER_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:ServiceProviderConfig";
const RESOURCE_TYPE_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:ResourceType";
const SCHEMA_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:Schema";

/// One user as the IdP reads it. `base_url` is the `/scim/v2` URL without a trailing slash.
pub(crate) fn user_resource(row: &ScimUserRow, base_url: &str) -> Value {
    let id = row.id.to_string();
    let enterprise = row.department.is_some() || row.employee_number.is_some();
    let mut schemas = vec![USER_SCHEMA];
    if enterprise {
        schemas.push(ENTERPRISE_USER);
    }

    let mut resource = Map::new();
    resource.insert("schemas".to_owned(), json!(schemas));
    resource.insert("id".to_owned(), json!(id));
    insert_optional(&mut resource, "externalId", row.external_id.as_deref());
    resource.insert("userName".to_owned(), json!(row.user_name));

    let mut name = Map::new();
    insert_optional(&mut name, "givenName", row.given_name.as_deref());
    insert_optional(&mut name, "familyName", row.family_name.as_deref());
    if !name.is_empty() {
        resource.insert("name".to_owned(), Value::Object(name));
    }
    insert_optional(&mut resource, "displayName", row.display_name.as_deref());
    resource.insert("active".to_owned(), json!(row.active));
    resource.insert(
        "emails".to_owned(),
        json!([{"value": row.user_name, "type": "work", "primary": true}]),
    );

    if enterprise {
        let mut extension = Map::new();
        insert_optional(&mut extension, "department", row.department.as_deref());
        insert_optional(
            &mut extension,
            "employeeNumber",
            row.employee_number.as_deref(),
        );
        resource.insert(ENTERPRISE_USER.to_owned(), Value::Object(extension));
    }

    resource.insert(
        "meta".to_owned(),
        json!({
            "resourceType": "User",
            "created": rfc3339(row.created_at),
            "lastModified": rfc3339(row.updated_at),
            "location": format!("{base_url}/Users/{id}"),
        }),
    );
    Value::Object(resource)
}

/// A ListResponse. `itemsPerPage` is the number of resources in this page.
pub(crate) fn list_response(total: u64, start_index: u32, resources: &[Value]) -> Value {
    let items = resources.len();
    json!({
        "schemas": [LIST_SCHEMA],
        "totalResults": total,
        "startIndex": start_index,
        "itemsPerPage": items,
        "Resources": resources,
    })
}

/// The ServiceProviderConfig. It advertises only what the server does. The `base_url` is not
/// used by this document.
pub(crate) fn service_provider_config(_base_url: &str) -> Value {
    json!({
        "schemas": [SERVICE_PROVIDER_SCHEMA],
        "patch": {"supported": true},
        "bulk": {"supported": false, "maxOperations": 0, "maxPayloadSize": 0},
        "filter": {"supported": true, "maxResults": 200},
        "changePassword": {"supported": false},
        "sort": {"supported": false},
        "etag": {"supported": false},
        "authenticationSchemes": [{
            "type": "oauthbearertoken",
            "name": "OAuth Bearer Token",
            "description": "A scim-cetra- token from Org settings → Provisioning.",
            "primary": true,
        }],
    })
}

/// The ResourceTypes ListResponse. M5 has the User type only.
pub(crate) fn resource_types(base_url: &str) -> Value {
    let user = json!({
        "schemas": [RESOURCE_TYPE_SCHEMA],
        "id": "User",
        "name": "User",
        "endpoint": "/Users",
        "schema": USER_SCHEMA,
        "schemaExtensions": [{"schema": ENTERPRISE_USER, "required": false}],
        "meta": {
            "resourceType": "ResourceType",
            "location": format!("{base_url}/ResourceTypes/User"),
        },
    });
    list_response(1, 1, &[user])
}

/// The Schemas ListResponse: the core User and the enterprise User, with the attributes cetra
/// supports.
pub(crate) fn schemas() -> Value {
    let core = json!({
        "schemas": [SCHEMA_SCHEMA],
        "id": USER_SCHEMA,
        "name": "User",
        "description": "User account.",
        "attributes": [
            attribute("userName", "string", true),
            {
                "name": "name",
                "type": "complex",
                "multiValued": false,
                "required": false,
                "mutability": "readWrite",
                "returned": "default",
                "uniqueness": "none",
                "subAttributes": [
                    attribute("givenName", "string", false),
                    attribute("familyName", "string", false),
                ],
            },
            attribute("displayName", "string", false),
            attribute("externalId", "string", false),
            attribute("active", "boolean", false),
            {
                "name": "emails",
                "type": "complex",
                "multiValued": true,
                "required": false,
                "mutability": "readWrite",
                "returned": "default",
                "uniqueness": "none",
                "subAttributes": [
                    attribute("value", "string", false),
                    attribute("type", "string", false),
                    attribute("primary", "boolean", false),
                ],
            },
        ],
    });
    let enterprise = json!({
        "schemas": [SCHEMA_SCHEMA],
        "id": ENTERPRISE_USER,
        "name": "EnterpriseUser",
        "description": "Enterprise user attributes.",
        "attributes": [
            attribute("department", "string", false),
            attribute("employeeNumber", "string", false),
        ],
    });
    list_response(2, 1, &[core, enterprise])
}

/// One simple attribute of a schema.
fn attribute(name: &str, kind: &str, required: bool) -> Value {
    json!({
        "name": name,
        "type": kind,
        "multiValued": false,
        "required": required,
        "caseExact": false,
        "mutability": "readWrite",
        "returned": "default",
        "uniqueness": "none",
    })
}

fn insert_optional(map: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        map.insert(key.to_owned(), json!(value));
    }
}

/// An i64 of nanoseconds since the Unix epoch as RFC 3339 in UTC with milliseconds. An out-of-range
/// value gives an empty string, which no stored row produces.
fn rfc3339(ns: i64) -> String {
    let secs = ns.div_euclid(1_000_000_000);
    // `rem_euclid` by 10^9 is below 10^9, which fits in a u32.
    let nanos = u32::try_from(ns.rem_euclid(1_000_000_000)).unwrap_or(0);
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, nanos).map_or_else(String::new, |time| {
        time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    })
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    const BASE: &str = "https://cetra.example/scim/v2";

    fn row(department: Option<&str>) -> ScimUserRow {
        ScimUserRow {
            id: Uuid::from_u128(1),
            user_id: Uuid::from_u128(3),
            user_name: "ada@example.com".to_owned(),
            external_id: None,
            given_name: None,
            family_name: None,
            display_name: None,
            department: department.map(str::to_owned),
            employee_number: None,
            active: true,
            // 2023-11-14T22:13:20Z and 500 ms later.
            created_at: 1_700_000_000_000_000_000,
            updated_at: 1_700_000_001_500_000_000,
        }
    }

    #[test]
    fn user_resource_omits_absent_values() {
        let resource = user_resource(&row(None), BASE);
        assert_eq!(resource["schemas"], json!([USER_SCHEMA]));
        assert_eq!(resource["id"], "00000000-0000-0000-0000-000000000001");
        assert_eq!(resource["userName"], "ada@example.com");
        assert_eq!(resource["active"], true);
        assert_eq!(
            resource["emails"],
            json!([{"value": "ada@example.com", "type": "work", "primary": true}])
        );
        assert!(resource.get("externalId").is_none());
        assert!(resource.get("name").is_none());
        assert!(resource.get("displayName").is_none());
        assert_eq!(resource["meta"]["resourceType"], "User");
        assert_eq!(resource["meta"]["created"], "2023-11-14T22:13:20.000Z");
        assert_eq!(resource["meta"]["lastModified"], "2023-11-14T22:13:21.500Z");
        assert_eq!(
            resource["meta"]["location"],
            "https://cetra.example/scim/v2/Users/00000000-0000-0000-0000-000000000001"
        );
    }

    #[test]
    fn user_resource_lists_the_enterprise_schema_only_when_used() {
        let plain = user_resource(&row(None), BASE);
        assert_eq!(plain["schemas"].as_array().map(Vec::len), Some(1));
        assert!(plain.get(ENTERPRISE_USER).is_none());

        let managed = user_resource(&row(Some("Engineering")), BASE);
        assert_eq!(managed["schemas"], json!([USER_SCHEMA, ENTERPRISE_USER]));
        assert_eq!(managed[ENTERPRISE_USER]["department"], "Engineering");
        assert!(managed[ENTERPRISE_USER].get("employeeNumber").is_none());
    }

    #[test]
    fn list_response_shape() {
        let list = list_response(5, 2, &[json!({"id": "a"})]);
        assert_eq!(list["schemas"], json!([LIST_SCHEMA]));
        assert_eq!(list["totalResults"], 5);
        assert_eq!(list["startIndex"], 2);
        assert_eq!(list["itemsPerPage"], 1);
        assert_eq!(list["Resources"], json!([{"id": "a"}]));
    }

    #[test]
    fn service_provider_config_advertises_exactly_what_is_supported() {
        let config = service_provider_config(BASE);
        assert_eq!(config["patch"]["supported"], true);
        assert_eq!(config["bulk"]["supported"], false);
        assert_eq!(config["filter"]["supported"], true);
        assert_eq!(config["filter"]["maxResults"], 200);
        assert_eq!(config["changePassword"]["supported"], false);
        assert_eq!(config["sort"]["supported"], false);
        assert_eq!(config["etag"]["supported"], false);
        assert_eq!(
            config["authenticationSchemes"][0]["type"],
            "oauthbearertoken"
        );
    }

    #[test]
    fn resource_types_lists_only_users() {
        let types = resource_types(BASE);
        assert_eq!(types["totalResults"], 1);
        assert_eq!(types["Resources"][0]["endpoint"], "/Users");
    }

    #[test]
    fn schemas_lists_the_core_and_the_enterprise_user() {
        let all = schemas();
        assert_eq!(all["totalResults"], 2);
        assert_eq!(all["Resources"][0]["id"], USER_SCHEMA);
        assert_eq!(all["Resources"][1]["id"], ENTERPRISE_USER);
    }
}
