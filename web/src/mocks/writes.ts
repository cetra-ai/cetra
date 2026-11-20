import { type HttpHandler, HttpResponse, http } from "msw";
import type { Schema } from "@/api/types";
import datasetItemDocument from "../../../fixtures/api/responses/datasets/01900000-0000-7000-8000-000000000c01/items/01900000-0000-7000-8000-000000000d02.json";
import datasetVersionsDocument from "../../../fixtures/api/responses/datasets/01900000-0000-7000-8000-000000000c01/versions.json";
import datasetDocument from "../../../fixtures/api/responses/datasets/01900000-0000-7000-8000-000000000c01.json";
import adminUserCreatedDocument from "../../../fixtures/api/static/admin-user-created.json";
import adminUsersDocument from "../../../fixtures/api/static/admin-users.json";
import alertRulesDocument from "../../../fixtures/api/static/alert-rules.json";
import annotationQueueDocument from "../../../fixtures/api/static/annotation-queue.json";
import annotationQueueItemDocument from "../../../fixtures/api/static/annotation-queue-item.json";
import apiKeyCreatedDocument from "../../../fixtures/api/static/api-key-created.json";
import backfillDocument from "../../../fixtures/api/static/backfill.json";
import backfillEstimateDocument from "../../../fixtures/api/static/backfill-estimate.json";
import channelsDocument from "../../../fixtures/api/static/channels.json";
import datasetImportPreviewDocument from "../../../fixtures/api/static/dataset-import-preview.json";
import datasetImportResultDocument from "../../../fixtures/api/static/dataset-import-result.json";
import datasetItemFromTraceDocument from "../../../fixtures/api/static/dataset-item-from-trace.json";
import datasetItemsDeletedDocument from "../../../fixtures/api/static/dataset-items-deleted.json";
import datasetItemsEditedDocument from "../../../fixtures/api/static/dataset-items-edited.json";
import datasetItemsFromTracesDocument from "../../../fixtures/api/static/dataset-items-from-traces.json";
import datasetRestoredDocument from "../../../fixtures/api/static/dataset-restored.json";
import deletionOrgDocument from "../../../fixtures/api/static/deletion-org.json";
import deletionPreviewDocument from "../../../fixtures/api/static/deletion-preview.json";
import deletionProjectDocument from "../../../fixtures/api/static/deletion-project.json";
import deletionResolvingDocument from "../../../fixtures/api/static/deletion-resolving.json";
import evalJobQueuedDocument from "../../../fixtures/api/static/eval-job-queued.json";
import evaluatorDocument from "../../../fixtures/api/static/evaluator.json";
import evaluatorTestResultDocument from "../../../fixtures/api/static/evaluator-test-result.json";
import experimentDocument from "../../../fixtures/api/static/experiment.json";
import inviteCreatedDocument from "../../../fixtures/api/static/invite-created.json";
import llmConnectionDocument from "../../../fixtures/api/static/llm-connection.json";
import llmConnectionTestDocument from "../../../fixtures/api/static/llm-connection-test.json";
import meDocument from "../../../fixtures/api/static/me.json";
import modelPriceDocument from "../../../fixtures/api/static/model-price.json";
import onlineRuleDocument from "../../../fixtures/api/static/online-rule.json";
import onlineRulePreviewDocument from "../../../fixtures/api/static/online-rule-preview.json";
import onlineRuleResumedDocument from "../../../fixtures/api/static/online-rule-resumed.json";
import orgDocument from "../../../fixtures/api/static/org.json";
import orgMembersDocument from "../../../fixtures/api/static/org-members.json";
import orgModelPricesDocument from "../../../fixtures/api/static/org-model-prices.json";
import orgSsoDocument from "../../../fixtures/api/static/org-sso.json";
import personalTokenCreatedDocument from "../../../fixtures/api/static/personal-token-created.json";
import projectDocument from "../../../fixtures/api/static/project.json";
import projectMembersDocument from "../../../fixtures/api/static/project-members.json";
import scoreDefinitionDocument from "../../../fixtures/api/static/score-definition.json";
import setupResponseDocument from "../../../fixtures/api/static/setup-response.json";
import signupResponseDocument from "../../../fixtures/api/static/signup-response.json";

/**
 * Stateless write answers for the dev server's browser worker (D-L-10). They
 * are never installed in the test server: page tests mock writes per test.
 */
export type WriteMethod = "post" | "patch" | "put" | "delete";

export interface WriteOperation {
  operationId: string;
  method: WriteMethod;
  /** OpenAPI path, with `{param}` segments. */
  path: string;
}

export const WRITE_OPERATIONS: readonly WriteOperation[] = [
  { operationId: "post_login", method: "post", path: "/api/auth/login" },
  { operationId: "post_logout", method: "post", path: "/api/auth/logout" },
  { operationId: "post_sso_discover", method: "post", path: "/api/auth/sso/discover" },
  { operationId: "post_signup", method: "post", path: "/api/auth/signup" },
  { operationId: "post_setup", method: "post", path: "/api/auth/setup" },
  { operationId: "post_password_forgot", method: "post", path: "/api/auth/password/forgot" },
  { operationId: "post_password_reset", method: "post", path: "/api/auth/password/reset" },
  { operationId: "post_email_verify", method: "post", path: "/api/auth/email/verify" },
  {
    operationId: "post_email_verify_resend",
    method: "post",
    path: "/api/auth/email/verify/resend",
  },
  { operationId: "post_invite_accept", method: "post", path: "/api/auth/invites/{token}/accept" },
  { operationId: "patch_me", method: "patch", path: "/api/v1/me" },
  { operationId: "post_me_password", method: "post", path: "/api/v1/me/password" },
  { operationId: "post_personal_token", method: "post", path: "/api/v1/me/tokens" },
  { operationId: "delete_current_token", method: "delete", path: "/api/v1/me/tokens/current" },
  { operationId: "delete_personal_token", method: "delete", path: "/api/v1/me/tokens/{id}" },
  { operationId: "post_org", method: "post", path: "/api/v1/orgs" },
  { operationId: "patch_org", method: "patch", path: "/api/v1/orgs/{org}" },
  { operationId: "patch_org_member", method: "patch", path: "/api/v1/orgs/{org}/members/{user}" },
  { operationId: "delete_org_member", method: "delete", path: "/api/v1/orgs/{org}/members/{user}" },
  { operationId: "post_org_invite", method: "post", path: "/api/v1/orgs/{org}/invites" },
  { operationId: "delete_org_invite", method: "delete", path: "/api/v1/orgs/{org}/invites/{id}" },
  { operationId: "put_org_sso_provider", method: "put", path: "/api/v1/orgs/{org}/sso/provider" },
  {
    operationId: "delete_org_sso_provider",
    method: "delete",
    path: "/api/v1/orgs/{org}/sso/provider",
  },
  { operationId: "post_org_sso_domain", method: "post", path: "/api/v1/orgs/{org}/sso/domains" },
  {
    operationId: "post_org_sso_domain_verify",
    method: "post",
    path: "/api/v1/orgs/{org}/sso/domains/{id}/verify",
  },
  {
    operationId: "delete_org_sso_domain",
    method: "delete",
    path: "/api/v1/orgs/{org}/sso/domains/{id}",
  },
  { operationId: "patch_org_sso", method: "patch", path: "/api/v1/orgs/{org}/sso" },
  {
    operationId: "post_llm_connection",
    method: "post",
    path: "/api/v1/orgs/{org}/llm-connections",
  },
  {
    operationId: "patch_llm_connection",
    method: "patch",
    path: "/api/v1/orgs/{org}/llm-connections/{id}",
  },
  {
    operationId: "delete_llm_connection",
    method: "delete",
    path: "/api/v1/orgs/{org}/llm-connections/{id}",
  },
  {
    operationId: "post_llm_connection_test",
    method: "post",
    path: "/api/v1/orgs/{org}/llm-connections/{id}/test",
  },
  { operationId: "post_org_model_price", method: "post", path: "/api/v1/orgs/{org}/models" },
  {
    operationId: "patch_org_model_price",
    method: "patch",
    path: "/api/v1/orgs/{org}/models/{id}",
  },
  {
    operationId: "delete_org_model_price",
    method: "delete",
    path: "/api/v1/orgs/{org}/models/{id}",
  },
  { operationId: "post_project", method: "post", path: "/api/v1/orgs/{org}/projects" },
  { operationId: "patch_project", method: "patch", path: "/api/v1/projects/{project}" },
  { operationId: "delete_project", method: "delete", path: "/api/v1/projects/{project}" },
  { operationId: "delete_org", method: "delete", path: "/api/v1/orgs/{org}" },
  { operationId: "post_api_key", method: "post", path: "/api/v1/projects/{project}/api-keys" },
  {
    operationId: "delete_api_key",
    method: "delete",
    path: "/api/v1/projects/{project}/api-keys/{id}",
  },
  {
    operationId: "create_deletion",
    method: "post",
    path: "/api/v1/projects/{project}/deletions",
  },
  {
    operationId: "preview_deletion",
    method: "post",
    path: "/api/v1/projects/{project}/deletions/preview",
  },
  {
    operationId: "put_project_member",
    method: "put",
    path: "/api/v1/projects/{project}/members/{user}",
  },
  {
    operationId: "delete_project_member",
    method: "delete",
    path: "/api/v1/projects/{project}/members/{user}",
  },
  {
    operationId: "post_evaluator",
    method: "post",
    path: "/api/v1/projects/{project}/evaluators",
  },
  {
    operationId: "patch_evaluator",
    method: "patch",
    path: "/api/v1/projects/{project}/evaluators/{id}",
  },
  {
    operationId: "delete_evaluator",
    method: "delete",
    path: "/api/v1/projects/{project}/evaluators/{id}",
  },
  {
    operationId: "post_evaluator_test",
    method: "post",
    path: "/api/v1/projects/{project}/evaluators/test",
  },
  {
    operationId: "post_score_definition",
    method: "post",
    path: "/api/v1/projects/{project}/score-definitions",
  },
  {
    operationId: "patch_score_definition",
    method: "patch",
    path: "/api/v1/projects/{project}/score-definitions/{id}",
  },
  {
    operationId: "delete_score_definition",
    method: "delete",
    path: "/api/v1/projects/{project}/score-definitions/{id}",
  },
  { operationId: "post_model_price", method: "post", path: "/api/v1/projects/{project}/models" },
  {
    operationId: "patch_model_price",
    method: "patch",
    path: "/api/v1/projects/{project}/models/{id}",
  },
  {
    operationId: "delete_model_price",
    method: "delete",
    path: "/api/v1/projects/{project}/models/{id}",
  },
  { operationId: "post_admin_user", method: "post", path: "/api/v1/admin/users" },
  { operationId: "patch_admin_user", method: "patch", path: "/api/v1/admin/users/{id}" },
  {
    operationId: "post_admin_user_reset_link",
    method: "post",
    path: "/api/v1/admin/users/{id}/reset-link",
  },
  {
    operationId: "post_admin_settings_test_email",
    method: "post",
    path: "/api/v1/admin/settings/test-email",
  },
  { operationId: "create_channel", method: "post", path: "/api/v1/projects/{project}/channels" },
  {
    operationId: "update_channel",
    method: "patch",
    path: "/api/v1/projects/{project}/channels/{id}",
  },
  {
    operationId: "delete_channel",
    method: "delete",
    path: "/api/v1/projects/{project}/channels/{id}",
  },
  {
    operationId: "test_channel",
    method: "post",
    path: "/api/v1/projects/{project}/channels/{id}/test",
  },
  {
    operationId: "create_alert_rule",
    method: "post",
    path: "/api/v1/projects/{project}/alert-rules",
  },
  {
    operationId: "update_alert_rule",
    method: "patch",
    path: "/api/v1/projects/{project}/alert-rules/{id}",
  },
  {
    operationId: "delete_alert_rule",
    method: "delete",
    path: "/api/v1/projects/{project}/alert-rules/{id}",
  },
  {
    operationId: "post_online_rule",
    method: "post",
    path: "/api/v1/projects/{project}/online-rules",
  },
  {
    operationId: "patch_online_rule",
    method: "patch",
    path: "/api/v1/projects/{project}/online-rules/{id}",
  },
  {
    operationId: "delete_online_rule",
    method: "delete",
    path: "/api/v1/projects/{project}/online-rules/{id}",
  },
  {
    operationId: "post_online_rule_preview",
    method: "post",
    path: "/api/v1/projects/{project}/online-rules/preview",
  },
  {
    operationId: "post_online_rule_resume",
    method: "post",
    path: "/api/v1/projects/{project}/online-rules/{id}/resume",
  },
  {
    operationId: "post_backfill_estimate",
    method: "post",
    path: "/api/v1/projects/{project}/online-rules/{id}/backfills/estimate",
  },
  {
    operationId: "post_backfill",
    method: "post",
    path: "/api/v1/projects/{project}/online-rules/{id}/backfills",
  },
  {
    operationId: "post_evaluator_run",
    method: "post",
    path: "/api/v1/projects/{project}/evaluators/{id}/run",
  },
  { operationId: "create_scores", method: "post", path: "/api/v1/projects/{project}/scores" },
  {
    operationId: "delete_score",
    method: "delete",
    path: "/api/v1/projects/{project}/scores/{score_id}",
  },
  { operationId: "post_dataset", method: "post", path: "/api/v1/projects/{project}/datasets" },
  {
    operationId: "patch_dataset",
    method: "patch",
    path: "/api/v1/projects/{project}/datasets/{dataset}",
  },
  {
    operationId: "delete_dataset",
    method: "delete",
    path: "/api/v1/projects/{project}/datasets/{dataset}",
  },
  {
    operationId: "post_dataset_items",
    method: "post",
    path: "/api/v1/projects/{project}/datasets/{dataset}/items",
  },
  {
    operationId: "patch_dataset_item",
    method: "patch",
    path: "/api/v1/projects/{project}/datasets/{dataset}/items/{item_id}",
  },
  {
    operationId: "delete_dataset_item",
    method: "delete",
    path: "/api/v1/projects/{project}/datasets/{dataset}/items/{item_id}",
  },
  {
    operationId: "post_dataset_import_preview",
    method: "post",
    path: "/api/v1/projects/{project}/datasets/{dataset}/import/preview",
  },
  {
    operationId: "post_dataset_import",
    method: "post",
    path: "/api/v1/projects/{project}/datasets/{dataset}/import",
  },
  {
    operationId: "post_dataset_item_from_trace",
    method: "post",
    path: "/api/v1/projects/{project}/datasets/{dataset}/items/from-trace",
  },
  {
    operationId: "patch_dataset_items",
    method: "patch",
    path: "/api/v1/projects/{project}/datasets/{dataset}/items",
  },
  {
    operationId: "post_dataset_items_delete",
    method: "post",
    path: "/api/v1/projects/{project}/datasets/{dataset}/items/delete",
  },
  {
    operationId: "post_dataset_items_from_traces",
    method: "post",
    path: "/api/v1/projects/{project}/datasets/{dataset}/items/from-traces",
  },
  {
    operationId: "restore_dataset",
    method: "post",
    path: "/api/v1/projects/{project}/datasets/{dataset}/restore",
  },
  {
    operationId: "post_experiment",
    method: "post",
    path: "/api/v1/projects/{project}/experiments",
  },
  {
    operationId: "delete_experiment",
    method: "delete",
    path: "/api/v1/projects/{project}/experiments/{id}",
  },
  {
    operationId: "post_experiment_results",
    method: "post",
    path: "/api/v1/projects/{project}/experiments/{id}/items",
  },
  {
    operationId: "post_experiment_finish",
    method: "post",
    path: "/api/v1/projects/{project}/experiments/{id}/finish",
  },
  {
    operationId: "create_annotation_queue",
    method: "post",
    path: "/api/v1/projects/{project}/annotation-queues",
  },
  {
    operationId: "update_annotation_queue",
    method: "patch",
    path: "/api/v1/projects/{project}/annotation-queues/{id}",
  },
  {
    operationId: "delete_annotation_queue",
    method: "delete",
    path: "/api/v1/projects/{project}/annotation-queues/{id}",
  },
  {
    operationId: "add_annotation_queue_items",
    method: "post",
    path: "/api/v1/projects/{project}/annotation-queues/{id}/items",
  },
  {
    operationId: "remove_annotation_queue_item",
    method: "delete",
    path: "/api/v1/projects/{project}/annotation-queues/{id}/items/{item_id}",
  },
  {
    operationId: "next_annotation_queue_item",
    method: "post",
    path: "/api/v1/projects/{project}/annotation-queues/{id}/next",
  },
  {
    operationId: "lease_annotation_queue_item",
    method: "post",
    path: "/api/v1/projects/{project}/annotation-queues/{id}/items/{item_id}/lease",
  },
  {
    operationId: "release_annotation_queue_item",
    method: "delete",
    path: "/api/v1/projects/{project}/annotation-queues/{id}/items/{item_id}/lease",
  },
  {
    operationId: "complete_annotation_queue_item",
    method: "post",
    path: "/api/v1/projects/{project}/annotation-queues/{id}/items/{item_id}/complete",
  },
  {
    operationId: "skip_annotation_queue_item",
    method: "post",
    path: "/api/v1/projects/{project}/annotation-queues/{id}/items/{item_id}/skip",
  },
  {
    operationId: "reopen_annotation_queue_item",
    method: "post",
    path: "/api/v1/projects/{project}/annotation-queues/{id}/items/{item_id}/reopen",
  },
  {
    operationId: "preview_annotation_queue_matching",
    method: "post",
    path: "/api/v1/projects/{project}/annotation-queues/{id}/items/matching/preview",
  },
  {
    operationId: "add_annotation_queue_matching",
    method: "post",
    path: "/api/v1/projects/{project}/annotation-queues/{id}/items/matching",
  },
];

type Respond = (input: {
  body: Record<string, unknown>;
  params: Record<string, string>;
}) => Response;

const ORG_ROLES = ["owner", "admin", "member", "viewer"] as const;
const PROJECT_OVERRIDES = ["admin", "member", "viewer", "none"] as const;

const RESPONSES: Record<string, Respond> = {
  post_login: () => HttpResponse.json(meDocument),
  post_logout: () => noContent(),
  post_sso_discover: () => HttpResponse.json({ sso: false }),
  post_signup: () => HttpResponse.json(signupResponseDocument, { status: 201 }),
  post_setup: () => HttpResponse.json(setupResponseDocument, { status: 201 }),
  post_password_forgot: () => noContent(),
  post_password_reset: () => noContent(),
  post_email_verify: () => noContent(),
  post_email_verify_resend: () => noContent(),
  post_invite_accept: () => HttpResponse.json(orgDocument),

  patch_me: ({ body }) => {
    const { name: _name, ...unnamed } = meDocument.user;
    const name = typeof body.name === "string" ? body.name.trim() : "";
    return HttpResponse.json(name ? { ...unnamed, name } : unnamed);
  },
  post_me_password: () => noContent(),
  post_personal_token: ({ body }) => {
    const created = personalTokenCreatedDocument;
    const expiry = typeof body.expires_at === "string" ? new Date(body.expires_at) : undefined;
    return HttpResponse.json(
      {
        ...created,
        token: {
          ...created.token,
          name: typeof body.name === "string" ? body.name : created.token.name,
          scopes: Array.isArray(body.scopes) ? body.scopes : created.token.scopes,
          ...(expiry !== undefined && !Number.isNaN(expiry.getTime())
            ? { expires_at: expiry.toISOString() }
            : {}),
        },
      },
      { status: 201 },
    );
  },
  create_deletion: () => HttpResponse.json(deletionResolvingDocument, { status: 202 }),
  delete_project: () => HttpResponse.json(deletionProjectDocument, { status: 202 }),
  delete_org: () => HttpResponse.json(deletionOrgDocument, { status: 202 }),
  preview_deletion: () => HttpResponse.json(deletionPreviewDocument),
  delete_current_token: () => noContent(),
  delete_personal_token: () => noContent(),

  post_org: ({ body }) =>
    HttpResponse.json(
      { ...orgDocument, name: trimmed(body.name) ?? orgDocument.name },
      { status: 201 },
    ),
  patch_org: ({ body }) =>
    HttpResponse.json({ ...orgDocument, name: trimmed(body.name) ?? orgDocument.name }),
  patch_org_member: ({ body, params }) => {
    const member = orgMembersDocument.data.find((row) => row.user.id === params.user);
    if (!member) {
      return problem(404, "not_found");
    }
    const role = ORG_ROLES.find((candidate) => candidate === body.role);
    if (!role) {
      return problem(400, "bad_request");
    }
    return HttpResponse.json({ ...member, role });
  },
  delete_org_member: () => noContent(),
  delete_org_invite: () => noContent(),
  put_org_sso_provider: () => HttpResponse.json(orgSsoDocument),
  delete_org_sso_provider: () => noContent(),
  post_org_sso_domain: () => HttpResponse.json(orgSsoDocument, { status: 201 }),
  post_org_sso_domain_verify: () => HttpResponse.json(orgSsoDocument),
  delete_org_sso_domain: () => noContent(),
  patch_org_sso: () => HttpResponse.json(orgSsoDocument),
  post_org_invite: ({ body }) =>
    HttpResponse.json(
      {
        ...inviteCreatedDocument,
        invite: {
          ...inviteCreatedDocument.invite,
          email: typeof body.email === "string" ? body.email : inviteCreatedDocument.invite.email,
          role: typeof body.role === "string" ? body.role : inviteCreatedDocument.invite.role,
        },
      },
      { status: 201 },
    ),
  post_llm_connection: ({ body }) =>
    HttpResponse.json({ ...llmConnectionDocument, ...named(body) }, { status: 201 }),
  patch_llm_connection: ({ body }) =>
    HttpResponse.json({ ...llmConnectionDocument, ...named(body) }),
  delete_llm_connection: () => noContent(),
  post_llm_connection_test: () => HttpResponse.json(llmConnectionTestDocument),
  post_org_model_price: ({ body }) =>
    HttpResponse.json({ ...orgModelPricesDocument.data[0], ...priced(body) }, { status: 201 }),
  patch_org_model_price: ({ body, params }) =>
    HttpResponse.json({ ...orgModelPricesDocument.data[0], ...priced(body), id: params.id }),
  delete_org_model_price: () => noContent(),
  post_project: ({ body }) =>
    HttpResponse.json(
      {
        ...projectDocument,
        name: trimmed(body.name) ?? projectDocument.name,
        ...(typeof body.retention_days === "number" ? { retention_days: body.retention_days } : {}),
      },
      { status: 201 },
    ),

  patch_project: ({ body }) => {
    const { retention_days: _retention, ...withoutRetention } = projectDocument;
    const result: Record<string, unknown> =
      "retention_days" in body && body.retention_days === null
        ? { ...withoutRetention }
        : { ...projectDocument };
    if (typeof body.retention_days === "number") {
      result.retention_days = body.retention_days;
    }
    const name = trimmed(body.name);
    if (name !== undefined) {
      result.name = name;
    }
    return HttpResponse.json(result);
  },
  post_api_key: ({ body }) => {
    const kind = body.kind === "public" ? "public" : "secret";
    const scopes = Array.isArray(body.scopes)
      ? body.scopes
      : kind === "public"
        ? ["feedback"]
        : ["ingest", "read", "write"];
    const { name: _name, ...unnamedKey } = apiKeyCreatedDocument.key;
    const name = trimmed(body.name);
    return HttpResponse.json(
      {
        key: {
          ...unnamedKey,
          kind,
          scopes,
          prefix: kind === "public" ? "pk-cetra-EXAMPL" : "sk-cetra-EXAMPL",
          ...(name !== undefined ? { name } : {}),
        },
        secret:
          kind === "public"
            ? "pk-cetra-EXAMPLE0EXAMPLE0EXAMPLE0EXAMPLE0"
            : apiKeyCreatedDocument.secret,
      },
      { status: 201 },
    );
  },
  delete_api_key: () => noContent(),
  put_project_member: ({ body, params }) => {
    const member = projectMembersDocument.data.find((row) => row.user.id === params.user);
    if (!member) {
      return problem(404, "not_found");
    }
    if (member.org_role === "owner" || member.org_role === "admin") {
      return problem(400, "bad_request", "owners and admins cannot be overridden");
    }
    const role = PROJECT_OVERRIDES.find((candidate) => candidate === body.role);
    if (!role) {
      return problem(400, "bad_request");
    }
    return HttpResponse.json({
      user: member.user,
      org_role: member.org_role,
      override: role,
      ...(role !== "none" ? { effective_role: role } : {}),
    });
  },
  delete_project_member: () => noContent(),
  post_evaluator: ({ body }) =>
    HttpResponse.json({ ...evaluatorDocument, ...named(body) }, { status: 201 }),
  patch_evaluator: ({ body }) => HttpResponse.json({ ...evaluatorDocument, ...named(body) }),
  delete_evaluator: () => noContent(),
  post_evaluator_test: () => HttpResponse.json(evaluatorTestResultDocument),
  post_score_definition: ({ body }) => {
    const request = body as Schema<"CreateScoreDefinitionRequest">;
    const direction = request.direction ?? "none";
    const issues = directionIssues({
      data_type: request.data_type,
      direction,
      min_value: request.min_value,
      max_value: request.max_value,
      categories: request.categories ?? [],
    });
    if (issues.length > 0) {
      return problem(400, "bad_request", undefined, issues);
    }
    return HttpResponse.json(
      { ...scoreDefinitionDocument, ...named(body), direction },
      { status: 201 },
    );
  },
  patch_score_definition: ({ body }) => {
    // The mock serves one definition, so a patch is checked against that definition's type and range.
    const request = body as Schema<"UpdateScoreDefinitionRequest">;
    const direction = request.direction ?? scoreDefinitionDocument.direction;
    const issues = directionIssues({
      data_type: scoreDefinitionDocument.data_type,
      direction,
      min_value: request.min_value ?? scoreDefinitionDocument.min_value,
      max_value: request.max_value ?? scoreDefinitionDocument.max_value,
      categories: request.categories ?? scoreDefinitionDocument.categories,
    });
    if (issues.length > 0) {
      return problem(400, "bad_request", undefined, issues);
    }
    return HttpResponse.json({ ...scoreDefinitionDocument, ...named(body), direction });
  },
  delete_score_definition: () => noContent(),
  post_model_price: ({ body }) =>
    HttpResponse.json({ ...modelPriceDocument, ...priced(body) }, { status: 201 }),
  patch_model_price: ({ body, params }) =>
    HttpResponse.json({ ...modelPriceDocument, ...priced(body), id: params.id }),
  delete_model_price: () => noContent(),

  post_admin_user: ({ body }) => {
    const { name: _name, ...unnamedUser } = adminUserCreatedDocument.user;
    const name = trimmed(body.name);
    return HttpResponse.json(
      {
        reset_url: adminUserCreatedDocument.reset_url,
        user: {
          ...unnamedUser,
          email: typeof body.email === "string" ? body.email : adminUserCreatedDocument.user.email,
          is_instance_admin: body.is_instance_admin === true,
          ...(name !== undefined ? { name } : {}),
        },
      },
      { status: 201 },
    );
  },
  patch_admin_user: ({ body, params }) => {
    if (params.id === meDocument.user.id) {
      return problem(400, "bad_request", "cannot change your own account");
    }
    const user = adminUsersDocument.data.find((row) => row.id === params.id);
    if (!user) {
      return problem(404, "not_found");
    }
    return HttpResponse.json({
      ...user,
      ...(typeof body.disabled === "boolean" ? { disabled: body.disabled } : {}),
      ...(typeof body.is_instance_admin === "boolean"
        ? { is_instance_admin: body.is_instance_admin }
        : {}),
    });
  },
  post_admin_user_reset_link: ({ params }) => {
    const user = adminUsersDocument.data.find((row) => row.id === params.id);
    if (!user) {
      return problem(404, "not_found");
    }
    return HttpResponse.json({ url: adminUserCreatedDocument.reset_url });
  },
  post_admin_settings_test_email: () => problem(400, "bad_request", "email is not configured"),

  create_channel: ({ body }) => {
    const sample =
      channelsDocument.data.find((row) => row.kind === body.kind) ??
      channelsDocument.data.find(() => true);
    return HttpResponse.json(
      { ...sample, name: trimmed(body.name) ?? sample?.name },
      { status: 201 },
    );
  },
  update_channel: ({ body, params }) => {
    const channel =
      channelsDocument.data.find((row) => row.id === params.id) ??
      channelsDocument.data.find(() => true);
    return HttpResponse.json({ ...channel, name: trimmed(body.name) ?? channel?.name });
  },
  delete_channel: () => noContent(),
  test_channel: () => HttpResponse.json({ ok: true }),
  create_alert_rule: ({ body }) => {
    const sample = alertRulesDocument.data.find((row) => row.name === "High error rate");
    if (!sample) {
      return problem(404, "not_found");
    }
    const {
      last_count: _count,
      last_evaluated_at: _evaluated,
      last_fired_at: _fired,
      last_value: _value,
      ...rest
    } = sample;
    return HttpResponse.json(
      {
        ...rest,
        name: trimmed(body.name) ?? rest.name,
        condition: body.condition ?? rest.condition,
        channel_ids: Array.isArray(body.channel_ids) ? body.channel_ids : rest.channel_ids,
        enabled: typeof body.enabled === "boolean" ? body.enabled : rest.enabled,
        cooldown_s: typeof body.cooldown_s === "number" ? body.cooldown_s : rest.cooldown_s,
        notify_while_firing:
          typeof body.notify_while_firing === "boolean"
            ? body.notify_while_firing
            : rest.notify_while_firing,
        state: "ok",
      },
      { status: 201 },
    );
  },
  update_alert_rule: ({ body, params }) => {
    const rule =
      alertRulesDocument.data.find((row) => row.id === params.id) ??
      alertRulesDocument.data.find(() => true);
    return HttpResponse.json({ ...rule, ...body });
  },
  delete_alert_rule: () => noContent(),

  post_online_rule: ({ body }) =>
    HttpResponse.json({ ...onlineRuleDocument, ...named(body) }, { status: 201 }),
  patch_online_rule: ({ body }) => HttpResponse.json({ ...onlineRuleDocument, ...named(body) }),
  delete_online_rule: () => noContent(),
  post_online_rule_preview: () => HttpResponse.json(onlineRulePreviewDocument),
  post_online_rule_resume: ({ params }) =>
    HttpResponse.json({ ...onlineRuleResumedDocument, id: params.id }),
  post_backfill_estimate: () => HttpResponse.json(backfillEstimateDocument),
  post_backfill: () => HttpResponse.json(backfillDocument, { status: 202 }),
  post_evaluator_run: () => HttpResponse.json(evalJobQueuedDocument, { status: 202 }),
  create_scores: ({ body }) => {
    const scores = Array.isArray(body.scores) ? body.scores : [];
    return HttpResponse.json({
      score_ids: scores.map(() => "01900000-0000-7000-8000-000000000310"),
    });
  },
  delete_score: () => noContent(),

  post_dataset: ({ body }) =>
    HttpResponse.json({ ...datasetDocument, ...named(body) }, { status: 201 }),
  patch_dataset: ({ body }) => HttpResponse.json({ ...datasetDocument, ...named(body) }),
  delete_dataset: () => noContent(),
  post_dataset_items: () =>
    HttpResponse.json(
      { version: datasetDocument.current_version + 1, item_ids: [datasetItemDocument.id] },
      { status: 201 },
    ),
  patch_dataset_item: ({ params }) =>
    HttpResponse.json({ ...datasetItemDocument, id: params.item_id }),
  delete_dataset_item: () => HttpResponse.json(datasetVersionsDocument.data[0]),
  post_dataset_import_preview: () => HttpResponse.json(datasetImportPreviewDocument),
  post_dataset_import: () => HttpResponse.json(datasetImportResultDocument, { status: 201 }),
  post_dataset_item_from_trace: () =>
    HttpResponse.json(datasetItemFromTraceDocument, { status: 201 }),
  patch_dataset_items: () => HttpResponse.json(datasetItemsEditedDocument),
  post_dataset_items_delete: () => HttpResponse.json(datasetItemsDeletedDocument),
  post_dataset_items_from_traces: () =>
    HttpResponse.json(datasetItemsFromTracesDocument, { status: 201 }),
  restore_dataset: () => HttpResponse.json(datasetRestoredDocument),
  post_experiment: ({ body }) =>
    HttpResponse.json(
      {
        experiment: { ...experimentDocument, ...named(body) },
        items: [],
        next_cursor: null,
      },
      { status: 201 },
    ),
  delete_experiment: () => noContent(),
  post_experiment_results: ({ body }) =>
    HttpResponse.json({
      stored: Array.isArray(body.results) ? body.results.length : 0,
      skipped: 0,
    }),
  post_experiment_finish: ({ params }) =>
    HttpResponse.json({ ...experimentDocument, id: params.id, status: "completed" }),
  create_annotation_queue: ({ body }) =>
    HttpResponse.json(
      { ...annotationQueueDocument, name: trimmed(body.name) ?? annotationQueueDocument.name },
      { status: 201 },
    ),
  update_annotation_queue: ({ body }) =>
    HttpResponse.json({
      ...annotationQueueDocument,
      ...body,
      version: annotationQueueDocument.version + 1,
    }),
  delete_annotation_queue: () => noContent(),
  add_annotation_queue_items: ({ body }) =>
    HttpResponse.json(
      {
        added: Array.isArray(body.items) ? body.items.length : 0,
        existing: 0,
        skipped: [],
      },
      { status: 201 },
    ),
  remove_annotation_queue_item: () => noContent(),
  next_annotation_queue_item: () => HttpResponse.json(annotationQueueItemDocument),
  lease_annotation_queue_item: () => HttpResponse.json(annotationQueueItemDocument),
  release_annotation_queue_item: () => noContent(),
  complete_annotation_queue_item: () =>
    HttpResponse.json({ ...annotationQueueItemDocument, status: "completed" }),
  skip_annotation_queue_item: () =>
    HttpResponse.json({ ...annotationQueueItemDocument, status: "skipped" }),
  reopen_annotation_queue_item: () =>
    HttpResponse.json({ ...annotationQueueItemDocument, status: "pending" }),
  preview_annotation_queue_matching: () => HttpResponse.json({ matched: 42, capped: false }),
  add_annotation_queue_matching: () =>
    HttpResponse.json({ added: 42, existing: 0, blocked: 0, capped: false }, { status: 201 }),
};

export const writeHandlers: HttpHandler[] = WRITE_OPERATIONS.map((operation) => {
  const respond = RESPONSES[operation.operationId];
  if (!respond) {
    throw new Error(`writes: no response for ${operation.operationId}`);
  }
  const path = operation.path.replace(/\{([^}]+)\}/g, ":$1");
  return http[operation.method](path, async ({ request, params }) =>
    respond({ body: (await readJson(request)) ?? {}, params: stringParams(params) }),
  );
});

function noContent(): Response {
  return new HttpResponse(null, { status: 204 });
}

/** The trimmed value when it is a non-empty string, else `undefined`. */
function trimmed(value: unknown): string | undefined {
  if (typeof value !== "string" || value.trim().length === 0) {
    return undefined;
  }
  return value.trim();
}

/** The connection fixture's overrides, with `name` from the body when it is a string. */
function named(body: Record<string, unknown>): { name: string } | Record<string, never> {
  return typeof body.name === "string" ? { name: body.name } : {};
}

interface DirectionTarget {
  data_type: string;
  direction: string;
  min_value?: number;
  max_value?: number;
  categories: { value?: number }[];
}

/** The direction checks of spec 09 for a definition, with the server's messages (SD-3). */
function directionIssues(target: DirectionTarget): FieldIssue[] {
  if (target.direction === "none") {
    return [];
  }
  let message: string | undefined;
  if (target.data_type === "text") {
    message = "a text score has no direction";
  } else if (
    target.data_type === "numeric" &&
    (target.min_value === undefined || target.max_value === undefined)
  ) {
    message = "direction needs min_value and max_value";
  } else if (
    target.data_type === "categorical" &&
    target.categories.some((category) => category.value === undefined)
  ) {
    message = "direction needs a value on every category";
  }
  return message === undefined ? [] : [{ field: "direction", message }];
}

/** The price fixture's overrides, with the trimmed `model_name` from the body when it is one. */
function priced(body: Record<string, unknown>): { model_name: string } | Record<string, never> {
  const name = trimmed(body.model_name);
  return name ? { model_name: name } : {};
}

const TITLES: Record<400 | 404, string> = { 400: "Bad Request", 404: "Not Found" };

interface FieldIssue {
  field: string;
  message: string;
}

function problem(
  status: 400 | 404,
  code: string,
  detail?: string,
  errors: FieldIssue[] = [],
): Response {
  return HttpResponse.json(
    {
      type: "about:blank",
      title: TITLES[status],
      status,
      code,
      ...(detail !== undefined ? { detail } : {}),
      ...(errors.length > 0 ? { errors } : {}),
    },
    { status, headers: { "Content-Type": "application/problem+json" } },
  );
}

function stringParams(params: Record<string, unknown>): Record<string, string> {
  const pathParams: Record<string, string> = {};
  for (const [key, value] of Object.entries(params)) {
    if (typeof value === "string") {
      pathParams[key] = value;
    } else if (Array.isArray(value) && typeof value[0] === "string") {
      pathParams[key] = value[0];
    }
  }
  return pathParams;
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

async function readJson(request: Request): Promise<Record<string, unknown> | undefined> {
  try {
    const body: unknown = await request.json();
    return isObject(body) ? body : undefined;
  } catch {
    return undefined;
  }
}
