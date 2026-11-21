import { useQueryClient } from "@tanstack/react-query";
import { createFileRoute, Link } from "@tanstack/react-router";
import { useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage, problemOf } from "@/api/problem";
import { orgAssistantInit, orgAssistantKey, orgAssistantQuery } from "@/api/queries-assistant";
import { connectionsInit } from "@/api/queries-settings";
import type { Schema } from "@/api/types";
import { FormError } from "@/components/form/form-error";
import { TextField } from "@/components/form/text-field";
import { RelativeTime } from "@/components/relative-time";
import { ConnectionTestResult } from "@/components/settings/connection-test";
import { SettingsFormCard } from "@/components/settings/settings-card";
import { SettingsPage } from "@/components/settings/settings-page";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { canManageConnections } from "@/lib/roles";

type Connection = Schema<"LlmConnection">;

export const Route = createFileRoute("/$org/settings/assistant")({
  loader: ({ context, params }) =>
    Promise.all([
      context.queryClient.ensureQueryData(orgAssistantQuery(params.org)),
      context.queryClient.ensureQueryData(
        $api.queryOptions("get", "/api/v1/orgs/{org}/llm-connections", connectionsInit(params.org)),
      ),
    ]),
  component: OrgAssistantPage,
});

function OrgAssistantPage() {
  const { org } = Route.useParams();
  const { data: me } = $api.useSuspenseQuery("get", "/api/auth/me");
  const canManage = canManageConnections(me.orgs.find((item) => item.slug === org)?.role);
  const { data: setting } = $api.useSuspenseQuery(
    "get",
    "/api/v1/orgs/{org}/assistant",
    orgAssistantInit(org),
  );
  const { data: connections } = $api.useSuspenseQuery(
    "get",
    "/api/v1/orgs/{org}/llm-connections",
    connectionsInit(org),
  );
  return (
    <SettingsPage
      title="Assistant"
      description="The in-app assistant answers questions about this organization's data with one of its LLM connections."
    >
      <AssistantForm
        org={org}
        setting={setting}
        connections={connections.data}
        canManage={canManage}
      />
    </SettingsPage>
  );
}

function AssistantForm({
  org,
  setting,
  connections,
  canManage,
}: {
  org: string;
  setting: Schema<"AssistantSettings">;
  connections: Connection[];
  canManage: boolean;
}) {
  const queryClient = useQueryClient();
  const [enabled, setEnabled] = useState(setting.enabled);
  const [connectionId, setConnectionId] = useState(setting.connection_id ?? "");
  const [model, setModel] = useState(setting.model ?? "");
  const [fieldErrs, setFieldErrs] = useState<Record<string, string>>({});
  const [error, setError] = useState<string | undefined>();
  const [saved, setSaved] = useState(false);
  const save = $api.useMutation("put", "/api/v1/orgs/{org}/assistant");
  const test = $api.useMutation("post", "/api/v1/orgs/{org}/assistant/test");

  const trimmedModel = model.trim();
  const dirty =
    enabled !== setting.enabled ||
    connectionId !== (setting.connection_id ?? "") ||
    trimmedModel !== (setting.model ?? "");
  const locked = !canManage || save.isPending;
  const testable =
    canManage && connectionId !== "" && trimmedModel !== "" && !test.isPending && !save.isPending;
  const testError = test.error
    ? problemOf(test.error)?.status === 429
      ? "Too many tests. Wait a minute."
      : problemMessage(test.error, "Could not run the test.")
    : undefined;

  // Any edit makes the last save, error and test outcome stale.
  function edited() {
    setSaved(false);
    setError(undefined);
    setFieldErrs({});
    save.reset();
    test.reset();
  }

  function changeConnection(next: string) {
    setConnectionId(next);
    // Prefill the model from the connection's default, but never over a model the user typed.
    if (trimmedModel === "") {
      setModel(connections.find((candidate) => candidate.id === next)?.default_model ?? "");
    }
    edited();
  }

  function runTest() {
    test.mutate({
      params: { path: { org } },
      body: { connection_id: connectionId, model: trimmedModel },
    });
  }

  function onSubmit() {
    edited();
    save.mutate(
      {
        params: { path: { org } },
        body: {
          enabled,
          ...(connectionId !== "" ? { connection_id: connectionId } : {}),
          ...(trimmedModel !== "" ? { model: trimmedModel } : {}),
        },
      },
      {
        onSuccess: () => {
          setSaved(true);
          void queryClient.invalidateQueries({ queryKey: orgAssistantKey(org) });
          void queryClient.invalidateQueries({
            queryKey: ["get", "/api/v1/projects/{project}/assistant"],
          });
        },
        onError: (caught) => {
          const fields = fieldErrors(caught);
          if (fields.connection_id !== undefined || fields.model !== undefined) {
            setFieldErrs(fields);
            return;
          }
          if (problemOf(caught)?.status === 403) {
            setError("Only admins and owners can change the assistant.");
            return;
          }
          setError(problemMessage(caught, "Could not save the assistant setting."));
        },
      },
    );
  }

  return (
    <SettingsFormCard
      title="Connection and model"
      dirty={dirty}
      pending={save.isPending}
      saved={saved}
      error={error}
      disabledReason={canManage ? undefined : "Only admins and owners can change the assistant."}
      onSubmit={onSubmit}
    >
      <div className="flex flex-col gap-2">
        <div className="flex items-center gap-2">
          <Switch
            id="assistant-enabled"
            checked={enabled}
            disabled={locked}
            onCheckedChange={(next) => {
              setEnabled(next);
              edited();
            }}
          />
          <Label htmlFor="assistant-enabled">Turn on the assistant</Label>
        </div>
        <p className="text-caption text-muted-foreground">
          The assistant reads traces, prompts, completions and attributes to answer. What it reads
          is sent to the connection's provider. Turning it on runs a test call that must return a
          tool call.
        </p>
      </div>
      <div className="flex flex-col gap-2">
        <Label htmlFor="assistant-connection">Connection</Label>
        <Select
          value={connectionId}
          onValueChange={changeConnection}
          disabled={locked || connections.length === 0}
        >
          <SelectTrigger
            id="assistant-connection"
            aria-invalid={fieldErrs.connection_id ? true : undefined}
            className="w-fit"
          >
            <SelectValue placeholder="Choose a connection" />
          </SelectTrigger>
          <SelectContent>
            {connections.map((connection) => (
              <SelectItem key={connection.id} value={connection.id}>
                {connection.name}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <FormError message={fieldErrs.connection_id} />
        {connections.length === 0 ? (
          <p className="text-body text-muted-foreground">
            Add an LLM connection first.{" "}
            <Link
              to="/$org/settings/llm-connections"
              params={{ org }}
              className="underline underline-offset-4"
            >
              Open LLM connections
            </Link>
          </p>
        ) : null}
      </div>
      <TextField
        id="assistant-model"
        label="Model"
        value={model}
        disabled={locked}
        error={fieldErrs.model}
        description="The model must support tool calling on this connection."
        onChange={(next) => {
          setModel(next);
          edited();
        }}
      />
      <div className="flex flex-col gap-3">
        <div>
          <Button type="button" variant="outline" disabled={!testable} onClick={runTest}>
            Test
          </Button>
        </div>
        {test.data ? <ConnectionTestResult result={test.data} /> : null}
        <FormError message={testError} />
      </div>
      {setting.updated_at === undefined ? null : (
        <p className="text-caption text-muted-foreground">
          Last changed <RelativeTime value={setting.updated_at} />
        </p>
      )}
    </SettingsFormCard>
  );
}
