import { $api } from "./client";
import { projectRef } from "./project-ref";
import type { Schema } from "./types";

export function onlineRulesInit(org: string, project: string) {
  return { params: { path: { project: projectRef(org, project) }, query: { limit: 100 } } };
}

/** Prefix of every online rule list query of the project, for invalidation. */
export function onlineRulesKey(org: string, project: string) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/online-rules", {
    params: { path: { project: projectRef(org, project) } },
  }).queryKey;
}

export function onlineRuleInit(org: string, project: string, ruleId: string) {
  return { params: { path: { project: projectRef(org, project), id: ruleId } } };
}

export function onlineRuleQuery(org: string, project: string, ruleId: string) {
  return $api.queryOptions(
    "get",
    "/api/v1/projects/{project}/online-rules/{id}",
    onlineRuleInit(org, project, ruleId),
  );
}

/** The key of one online rule's detail query, for invalidation. */
export function onlineRuleKey(org: string, project: string, ruleId: string) {
  return onlineRuleQuery(org, project, ruleId).queryKey;
}

/** A rule's backfills, newest first (the latest 50). */
export function backfillsQuery(org: string, project: string, ruleId: string) {
  return $api.queryOptions(
    "get",
    "/api/v1/projects/{project}/online-rules/{id}/backfills",
    onlineRuleInit(org, project, ruleId),
  );
}

/** The key of one rule's backfill list, for invalidation. */
export function backfillsKey(org: string, project: string, ruleId: string) {
  return backfillsQuery(org, project, ruleId).queryKey;
}

export function evalJobsInit(
  org: string,
  project: string,
  filters: {
    rule_id?: string;
    evaluator_id?: string;
    status?: Schema<"EvalJobStatus">;
    kind?: Schema<"EvalJobKind">;
  },
) {
  return {
    params: { path: { project: projectRef(org, project) }, query: { limit: 50, ...filters } },
  };
}

/** Prefix of every eval job list query of the project, for invalidation. */
export function evalJobsKey(org: string, project: string) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/eval-jobs", {
    params: { path: { project: projectRef(org, project) } },
  }).queryKey;
}

export function evalJobQuery(org: string, project: string, jobId: string) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/eval-jobs/{id}", {
    params: { path: { project: projectRef(org, project), id: jobId } },
  });
}
