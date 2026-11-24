# SCIM request fixtures

These files replay the requests that Okta and Microsoft Entra ID send to cetra's SCIM 2.0
endpoints (`/scim/v2/Users` and `/scim/v2/Groups`). The replay tests in
`crates/server/src/ee/scim/replay_tests.rs` send them to a licensed test server and check the
responses.

The requests are written from the vendors' public documentation of their SCIM clients. They are
**not** captured from a validator run: the vendors' validators cannot run from here. Replacing
them with captured requests is a follow-up. Until then, treat a passing replay as evidence of
the documented shapes, not of the vendors' exact traffic.

## Layout

- `okta/connection.json`: the Okta "test connection" request.
- `okta/user_lifecycle.json`: create, read, replace, deactivate and reactivate a user.
- `okta/groups.json`: create a group, add and remove a member, rename, delete.
- `entra/connection.json`: the Entra ID "test connection" lookup.
- `entra/user_lifecycle.json`: create with the enterprise extension, deactivate, change names
  and the enterprise `department`, delete.
- `entra/groups.json`: look a group up, create it, add and remove a member.

Each file is one ordered scenario: a JSON array of steps, run in order against one org.

## Step format

```json
[
  { "method": "POST", "path": "/scim/v2/Users", "body": { "...": "..." },
    "expect": { "status": 201, "save": { "user": "/id" } } },
  { "method": "PATCH", "path": "/scim/v2/Users/{user}", "body": { "...": "..." },
    "expect": { "status": 200, "json": { "/active": false } } }
]
```

- `method`, `path` and `body` are sent as `application/scim+json` with the bearer token of the
  org's SCIM token. `body` is omitted for requests without one.
- `expect.status` is the HTTP status the response must have.
- `expect.save` maps a name to a JSON pointer into the response body. The value is stored and
  replaces `{name}` in every later step.
- `expect.json` maps a JSON pointer into the response body to the value it must have.
- `expect.absent` lists JSON pointers that must not exist in the response body (for example a
  `members` key that `excludedAttributes=members` must leave out).
- `{name}` is replaced by plain text on the serialized step before it is parsed, so it works in
  paths, bodies and expected values alike.
- The test runner percent-encodes the values of query parameters, so `filter` values can carry
  spaces and quotes as they are written here.

## Conventions

- Emails use the test domain `acme.test`. The verified domain of the test org is `acme.test`.
- Every PATCH body carries `"schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"]`.
- Each vendor's own casing is kept: Okta sends `op` values in lower case and Entra ID in
  capitals (`Add`, `Replace`, `Remove`), and Entra ID sends `active` as the string `"False"`.
- Entra ID's enterprise attributes use the enterprise URN, both as an object in a body and as a
  path prefix in a PATCH (`urn:ietf:params:scim:schemas:extension:enterprise:2.0:User:department`).
- Member lists are arrays of `{ "value": "<SCIM user id>" }` objects.
- A `work` email that differs from the `userName` is ignored, as cetra's account email is the
  `userName` (D-EA-13). The Entra scenario sends one to check that the request still succeeds.
