// SPDX-License-Identifier: Elastic-2.0
import type { QueryClient } from "@tanstack/react-query";
import { orgSsoQuery } from "@/api/queries-settings";
import type { Schema } from "@/api/types";

/**
 * Stores what a settings write answered. A write that answers with the whole settings
 * (`OrgSso`) replaces the cache; one that answers with no body refetches them.
 */
export function storeOrgSso(
  queryClient: QueryClient,
  org: string,
  answer: Schema<"OrgSso"> | undefined,
) {
  if (answer) {
    queryClient.setQueryData(orgSsoQuery(org).queryKey, answer);
    return;
  }
  void queryClient.invalidateQueries({ queryKey: orgSsoQuery(org).queryKey });
}
