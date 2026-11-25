import { useQueryClient } from "@tanstack/react-query";
import { X } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage, problemOf } from "@/api/problem";
import { connectionsKey } from "@/api/queries-settings";
import type { Schema } from "@/api/types";
import { DisabledReason } from "@/components/disabled-reason";
import { FormError } from "@/components/form/form-error";
import { TextField } from "@/components/form/text-field";
import { ConnectionTestResult } from "@/components/settings/connection-test";
import { SettingsFormCard } from "@/components/settings/settings-card";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  adapterOf,
  DEFAULT_BASE_URLS,
  PROVIDER_CHOICES,
  type ProviderChoice,
  providerChoice,
} from "@/lib/llm-adapters";
import { MODEL_SUGGESTIONS } from "@/lib/llm-models";

type Connection = Schema<"LlmConnection">;
type StructuredMode = Schema<"StructuredOutputMode">;
type TestKind = "chat" | "embedding";

const STRUCTURED_OUTPUT_MODES: { value: StructuredMode; label: string }[] = [
  { value: "native", label: "Native (default)" },
  { value: "tool", label: "Tool call" },
  { value: "prompt", label: "Prompt" },
];

/** The label of a structured output mode, as the form and the read-only details show it. */
export function structuredOutputLabel(mode: StructuredMode): string {
  return STRUCTURED_OUTPUT_MODES.find((option) => option.value === mode)?.label ?? mode;
}

type FieldKey =
  | "name"
  | "api_key"
  | "base_url"
  | "default_model"
  | "max_concurrency"
  | "requests_per_minute"
  | "headers";

/** Fields the form renders; only server errors for these are placed under the field. */
const PLACED_FIELDS: ReadonlySet<string> = new Set<FieldKey>([
  "name",
  "api_key",
  "base_url",
  "default_model",
  "max_concurrency",
  "requests_per_minute",
  "headers",
]);

function isPlacedField(field: string): field is FieldKey {
  return PLACED_FIELDS.has(field);
}

interface HeaderRow {
  id: number;
  name: string;
  value: string;
}

let headerRowId = 0;

function newHeaderRow(name: string): HeaderRow {
  headerRowId += 1;
  return { id: headerRowId, name, value: "" };
}

function providerValue(value: string): ProviderChoice {
  return PROVIDER_CHOICES.find((option) => option.value === value)?.value ?? "openai";
}

function structuredValue(value: string): StructuredMode | "" {
  return value === "native" || value === "tool" || value === "prompt" ? value : "";
}

function isWholeNumberIn(text: string, min: number, max: number): boolean {
  const value = Number(text);
  return Number.isInteger(value) && value >= min && value <= max;
}

/** The first problem with the header rows, or undefined. Blank rows are ignored. */
function headerProblem(rows: HeaderRow[]): string | undefined {
  const seen = new Set<string>();
  let problem: string | undefined;
  for (const row of rows) {
    const rowName = row.name.trim();
    if (rowName.length === 0 && row.value.trim().length === 0) {
      continue;
    }
    if (rowName.length === 0 || row.value.trim().length === 0) {
      problem ??= "Enter a value for each header.";
      continue;
    }
    const lowered = rowName.toLowerCase();
    if (seen.has(lowered)) {
      problem ??= "Duplicate header name.";
    } else {
      seen.add(lowered);
    }
  }
  return problem;
}

/** Create and patch share one mutation hook each. The request holds the API key (D-L-5). */
function useConnectionMutations(org: string) {
  const queryClient = useQueryClient();
  const refresh = () => {
    void queryClient.invalidateQueries({ queryKey: connectionsKey(org) });
  };
  // gcTime 0 drops the request, with the key and the header values, from the cache once reset.
  // The list refresh sits on the hook, so a close mid-request cannot skip it.
  const create = $api.useMutation("post", "/api/v1/orgs/{org}/llm-connections", {
    gcTime: 0,
    onSuccess: refresh,
  });
  const patch = $api.useMutation("patch", "/api/v1/orgs/{org}/llm-connections/{id}", {
    gcTime: 0,
    onSuccess: refresh,
  });
  return { create, patch };
}

/**
 * The create or edit form of a connection, shown in the list's row panel. Save is enabled only
 * while the values differ and are valid. A create form calls `onCreated` once saved. Test checks
 * the form's current values, saved or not, and shows the result under the form (D-I-53).
 */
export function ConnectionForm({
  org,
  connection,
  onCreated,
}: {
  org: string;
  /** Undefined for a new connection. */
  connection: Connection | undefined;
  onCreated: () => void;
}) {
  const { create, patch } = useConnectionMutations(org);
  const queryClient = useQueryClient();
  // gcTime 0: a test request holds the key, so it leaves the cache once settled.
  const testSaved = $api.useMutation("post", "/api/v1/orgs/{org}/llm-connections/{id}/test", {
    gcTime: 0,
  });
  const testUnsaved = $api.useMutation("post", "/api/v1/orgs/{org}/llm-connections/test", {
    gcTime: 0,
  });
  const pending = connection === undefined ? create.isPending : patch.isPending;
  const [name, setName] = useState(connection?.name ?? "");
  const [provider, setProvider] = useState<ProviderChoice>(
    connection ? providerChoice(connection) : "openai",
  );
  const adapter = adapterOf(provider);
  const [apiKey, setApiKey] = useState("");
  const [baseUrl, setBaseUrl] = useState(connection?.base_url ?? "");
  const [defaultModel, setDefaultModel] = useState(connection?.default_model ?? "");
  // "" means the user picked nothing yet: the server default (native) applies.
  const [structured, setStructured] = useState<StructuredMode | "">(
    connection?.structured_output ?? "",
  );
  const [maxConcurrency, setMaxConcurrency] = useState(
    connection ? String(connection.max_concurrency) : "",
  );
  const [requestsPerMinute, setRequestsPerMinute] = useState(
    connection ? String(connection.requests_per_minute) : "",
  );
  const [rows, setRows] = useState<HeaderRow[]>(() =>
    (connection?.extra_header_names ?? []).map((headerName) => newHeaderRow(headerName)),
  );
  const [headersDirty, setHeadersDirty] = useState(false);
  const [advancedOpen, setAdvancedOpen] = useState(false);
  const [touched, setTouched] = useState<Partial<Record<string, true>>>({});
  const [serverErrors, setServerErrors] = useState<Partial<Record<FieldKey, string>>>({});
  const [serverError, setServerError] = useState<string | undefined>();
  const [saved, setSaved] = useState(false);
  const [testResult, setTestResult] = useState<Schema<"LlmConnectionTestResult"> | undefined>();
  const [testError, setTestError] = useState<string | undefined>();
  const [embeddingModel, setEmbeddingModel] = useState("");
  // Focus goes here when a test ends: the form's controls are disabled while it runs.
  const testRegion = useRef<HTMLDivElement>(null);
  const testing = testSaved.isPending || testUnsaved.isPending;
  // While a test runs the form cannot change, so its result always describes the values shown.
  const busy = pending || testing;

  const trimmedName = name.trim();
  // Header values are hidden on an edit, so the rows only count once the user changes them.
  const headersActive = connection === undefined || headersDirty;
  const clientErrors: Partial<Record<FieldKey, string>> = {};
  if (trimmedName.length === 0) {
    clientErrors.name = "Enter a name.";
  }
  if (connection === undefined && apiKey.trim().length === 0) {
    clientErrors.api_key = "Enter the API key.";
  }
  if (provider === "openai_compatible" && baseUrl.trim().length === 0) {
    clientErrors.base_url = "Enter the server's base URL.";
  }
  if (maxConcurrency.trim() !== "" && !isWholeNumberIn(maxConcurrency.trim(), 1, 64)) {
    clientErrors.max_concurrency = "Enter a whole number from 1 to 64.";
  }
  if (requestsPerMinute.trim() !== "" && !isWholeNumberIn(requestsPerMinute.trim(), 1, 10000)) {
    clientErrors.requests_per_minute = "Enter a whole number from 1 to 10000.";
  }
  const headerError = headersActive ? headerProblem(rows) : undefined;
  if (headerError !== undefined) {
    clientErrors.headers = headerError;
  }
  const invalid = Object.keys(clientErrors).length > 0;
  const suggestions = MODEL_SUGGESTIONS[provider];

  /** A client error shows once its field was touched; a server error shows until the next save. */
  function fieldError(field: FieldKey): string | undefined {
    if (touched[field] === true && clientErrors[field] !== undefined) {
      return clientErrors[field];
    }
    return serverErrors[field];
  }

  function touch(field: string) {
    setTouched((current) => (current[field] === true ? current : { ...current, [field]: true }));
    setSaved(false);
    // A result describes the values it tested.
    setTestResult(undefined);
    setTestError(undefined);
  }

  function headerValues(): Record<string, string> {
    const map: Record<string, string> = {};
    for (const row of rows) {
      const rowName = row.name.trim();
      if (rowName.length === 0 && row.value.trim().length === 0) {
        continue;
      }
      map[rowName] = row.value;
    }
    return map;
  }

  function chooseProvider(next: ProviderChoice) {
    setProvider(next);
    // The URL error was for the previous provider's URL.
    setServerErrors(({ base_url: _, ...rest }) => rest);
    // Another provider's URL is wrong for this one; the saved choice gets its saved URL back.
    setBaseUrl(
      connection !== undefined && next === providerChoice(connection)
        ? (connection.base_url ?? "")
        : "",
    );
    touch("provider");
  }

  function createBody(): Schema<"CreateLlmConnectionRequest"> {
    const body: Schema<"CreateLlmConnectionRequest"> = {
      name: trimmedName,
      adapter,
      api_key: apiKey,
    };
    if (baseUrl.trim()) {
      body.base_url = baseUrl.trim();
    }
    if (defaultModel.trim()) {
      body.default_model = defaultModel.trim();
    }
    if (structured !== "") {
      body.structured_output = structured;
    }
    if (maxConcurrency.trim()) {
      body.max_concurrency = Number(maxConcurrency);
    }
    if (requestsPerMinute.trim()) {
      body.requests_per_minute = Number(requestsPerMinute);
    }
    if (headersDirty) {
      body.extra_headers = headerValues();
    }
    return body;
  }

  /** Only the fields that differ from the saved connection. */
  function editBody(current: Connection): Schema<"UpdateLlmConnectionRequest"> {
    const body: Schema<"UpdateLlmConnectionRequest"> = {};
    if (trimmedName !== current.name) {
      body.name = trimmedName;
    }
    if (adapter !== current.adapter) {
      body.adapter = adapter;
    }
    if (apiKey.trim().length > 0) {
      body.api_key = apiKey;
    }
    const trimmedBaseUrl = baseUrl.trim();
    if (trimmedBaseUrl !== (current.base_url ?? "").trim()) {
      body.base_url = trimmedBaseUrl;
    }
    const trimmedModel = defaultModel.trim();
    if (trimmedModel !== (current.default_model ?? "").trim()) {
      body.default_model = trimmedModel;
    }
    if (structured !== "" && structured !== current.structured_output) {
      body.structured_output = structured;
    }
    if (maxConcurrency.trim() !== "" && Number(maxConcurrency) !== current.max_concurrency) {
      body.max_concurrency = Number(maxConcurrency);
    }
    if (
      requestsPerMinute.trim() !== "" &&
      Number(requestsPerMinute) !== current.requests_per_minute
    ) {
      body.requests_per_minute = Number(requestsPerMinute);
    }
    if (headersDirty) {
      body.extra_headers = headerValues();
    }
    return body;
  }

  const body = connection === undefined ? undefined : editBody(connection);
  const dirty =
    body === undefined
      ? trimmedName.length > 0 ||
        provider !== "openai" ||
        apiKey.length > 0 ||
        baseUrl.trim().length > 0 ||
        defaultModel.trim().length > 0 ||
        structured !== "" ||
        maxConcurrency.trim().length > 0 ||
        requestsPerMinute.trim().length > 0 ||
        rows.some((row) => row.name.trim().length > 0 || row.value.trim().length > 0)
      : Object.keys(body).length > 0;

  function handleError(caught: unknown) {
    const next: Partial<Record<FieldKey, string>> = {};
    const unplaced: string[] = [];
    for (const [field, message] of Object.entries(fieldErrors(caught))) {
      if (field.startsWith("extra_headers")) {
        next.headers ??= message;
      } else if (isPlacedField(field)) {
        next[field] = message;
      } else {
        // A field the form does not render: the user still has to see it.
        unplaced.push(message);
      }
    }
    setServerErrors(next);
    if (unplaced.length > 0) {
      setServerError(unplaced.join(" "));
    } else if (Object.keys(next).length === 0) {
      setServerError(problemMessage(caught, "Could not save the connection."));
    }
  }

  function submit() {
    setServerErrors({});
    setServerError(undefined);
    if (invalid) {
      return;
    }
    if (connection === undefined) {
      create.mutate(
        { params: { path: { org } }, body: createBody() },
        { onSuccess: () => onCreated(), onError: handleError },
      );
      return;
    }
    const sent = editBody(connection);
    patch.mutate(
      { params: { path: { org, id: connection.id } }, body: sent },
      {
        onSuccess: (updated) => {
          // The saved provider decides the select, as the header shows it (a base URL makes it compatible).
          setProvider(providerChoice(updated));
          // The saved key and header values are gone from the form; the names stay.
          setApiKey("");
          setHeadersDirty(false);
          setTouched({});
          // The saved values changed, so the last result no longer describes them.
          setTestResult(undefined);
          setTestError(undefined);
          if (sent.extra_headers !== undefined) {
            setRows(Object.keys(sent.extra_headers).map((headerName) => newHeaderRow(headerName)));
          }
          setSaved(true);
        },
        onError: handleError,
      },
    );
  }

  /** A saved connection's test: the fields that differ from it, and the model only when it changed. */
  function savedTestBody(current: Connection, kind: TestKind): Schema<"TestLlmConnectionRequest"> {
    const edits = editBody(current);
    const body: Schema<"TestLlmConnectionRequest"> = {};
    if (edits.adapter !== undefined) {
      body.adapter = edits.adapter;
    }
    if (edits.api_key !== undefined) {
      body.api_key = edits.api_key;
    }
    if (edits.base_url !== undefined) {
      body.base_url = edits.base_url;
    }
    if (edits.structured_output !== undefined) {
      body.structured_output = edits.structured_output;
    }
    if (edits.extra_headers !== undefined) {
      body.extra_headers = edits.extra_headers;
    }
    if (kind === "embedding") {
      body.embedding_model = embeddingModel.trim();
    } else if (defaultModel.trim() !== (current.default_model ?? "").trim()) {
      // An unchanged default model is left out, so the server counts the test as one of the
      // default model (D-I-53).
      body.model = defaultModel.trim();
    }
    return body;
  }

  /** An unsaved connection's test: every value set in the form. */
  function unsavedTestBody(kind: TestKind): Schema<"TestUnsavedLlmConnectionRequest"> {
    const body: Schema<"TestUnsavedLlmConnectionRequest"> = { adapter, api_key: apiKey };
    if (baseUrl.trim()) {
      body.base_url = baseUrl.trim();
    }
    if (structured !== "") {
      body.structured_output = structured;
    }
    if (headersDirty) {
      body.extra_headers = headerValues();
    }
    if (kind === "embedding") {
      body.embedding_model = embeddingModel.trim();
    } else {
      body.model = defaultModel.trim();
    }
    return body;
  }

  // Only what a test sends can stop it: the name and the limits are not sent.
  const testProblem = clientErrors.api_key ?? clientErrors.base_url ?? clientErrors.headers;
  const chatTestReason =
    testProblem ?? (defaultModel.trim() === "" ? "Enter a default model to test." : undefined);
  const embeddingTestReason =
    adapter === "anthropic"
      ? "Anthropic has no embeddings API"
      : (testProblem ??
        (embeddingModel.trim() === "" ? "Enter an embedding model to test." : undefined));

  /** A test's errors: a field the form renders goes under it, the rest under the form's result. */
  function handleTestError(caught: unknown) {
    if (problemOf(caught)?.status === 429) {
      setTestError("Too many tests. Wait a minute.");
      return;
    }
    const placed: Partial<Record<FieldKey, string>> = {};
    const unplaced: string[] = [];
    for (const [field, message] of Object.entries(fieldErrors(caught))) {
      if (field.startsWith("extra_headers")) {
        placed.headers ??= message;
      } else if (isPlacedField(field)) {
        placed[field] = message;
      } else {
        // `model`, `embedding_model`: the test's own fields, shown under the form.
        unplaced.push(message);
      }
    }
    setServerErrors(placed);
    if (unplaced.length > 0) {
      setTestError(unplaced.join(" "));
    } else if (Object.keys(placed).length === 0) {
      setTestError(problemMessage(caught, "Could not run the test."));
    }
  }

  function runTest(kind: TestKind) {
    setTestResult(undefined);
    setTestError(undefined);
    setServerErrors({});
    setServerError(undefined);
    const options = {
      onSuccess: (result: Schema<"LlmConnectionTestResult">) => {
        setTestResult(result);
      },
      onError: handleTestError,
    };
    if (connection === undefined) {
      testUnsaved.mutate({ params: { path: { org } }, body: unsavedTestBody(kind) }, options);
      return;
    }
    testSaved.mutate(
      { params: { path: { org, id: connection.id } }, body: savedTestBody(connection, kind) },
      {
        ...options,
        onSuccess: (result) => {
          setTestResult(result);
          // The status and the last test may have changed.
          void queryClient.invalidateQueries({ queryKey: connectionsKey(org) });
        },
      },
    );
  }

  function addHeaderRow() {
    setHeadersDirty(true);
    touch("headers");
    setRows((current) => [...current, newHeaderRow("")]);
  }

  function updateHeaderRow(id: number, change: Partial<HeaderRow>) {
    setHeadersDirty(true);
    touch("headers");
    setRows((current) => current.map((row) => (row.id === id ? { ...row, ...change } : row)));
  }

  function removeHeaderRow(id: number) {
    setHeadersDirty(true);
    touch("headers");
    setRows((current) => current.filter((row) => row.id !== id));
  }

  const advancedError =
    (provider !== "openai_compatible" && fieldError("base_url") !== undefined) ||
    fieldError("max_concurrency") !== undefined ||
    fieldError("requests_per_minute") !== undefined ||
    fieldError("headers") !== undefined;
  const headersError = fieldError("headers");

  // An error opens Advanced, where its field is; the user may close it again.
  useEffect(() => {
    if (advancedError) {
      setAdvancedOpen(true);
    }
  }, [advancedError]);

  // Focus moves to the test's outcome when a test ends. A cleared result does not move it.
  useEffect(() => {
    if (testResult !== undefined || testError !== undefined) {
      testRegion.current?.focus();
    }
  }, [testResult, testError]);

  return (
    <SettingsFormCard
      title="Connection"
      ariaLabel={connection === undefined ? "New connection" : "Edit connection"}
      dirty={dirty}
      invalid={invalid}
      pending={pending}
      saved={saved}
      error={serverError}
      saveLabel={connection === undefined ? "Add connection" : "Save"}
      disabledReason={testing ? "Wait for the test to finish." : undefined}
      onSubmit={submit}
      footerExtra={
        <DisabledReason reason={chatTestReason}>
          <Button
            type="button"
            variant="outline"
            disabled={chatTestReason !== undefined || testing || pending}
            onClick={() => runTest("chat")}
          >
            {testing ? "Testing…" : "Test connection"}
          </Button>
        </DisabledReason>
      }
    >
      <div className="flex max-w-xl flex-col gap-4">
        <div className="flex flex-col gap-2">
          <Label htmlFor="connection-provider">Provider</Label>
          <Select
            value={provider}
            onValueChange={(value) => {
              chooseProvider(providerValue(value));
            }}
            disabled={busy}
          >
            <SelectTrigger id="connection-provider" className="w-fit">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {PROVIDER_CHOICES.map((option) => (
                <SelectItem key={option.value} value={option.value}>
                  {option.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
        <TextField
          id="connection-name"
          label="Name"
          value={name}
          maxLength={100}
          error={fieldError("name")}
          disabled={busy}
          onChange={(next) => {
            setName(next);
            touch("name");
          }}
        />
        <TextField
          id="connection-api-key"
          label="API key"
          type="password"
          autoComplete="new-password"
          value={apiKey}
          placeholder={
            connection === undefined
              ? undefined
              : connection.api_key_hint
                ? `••••${connection.api_key_hint}`
                : "••••••••"
          }
          error={fieldError("api_key")}
          disabled={busy}
          description={
            connection === undefined
              ? "Stored encrypted, never shown again."
              : "Stored encrypted. Type a new key to replace it."
          }
          onChange={(next) => {
            setApiKey(next);
            touch("api_key");
          }}
        />
        {provider === "openai_compatible" ? (
          <TextField
            id="connection-base-url"
            label="Base URL"
            placeholder="https://my-gateway.example.com/v1"
            value={baseUrl}
            error={fieldError("base_url")}
            disabled={busy}
            onChange={(next) => {
              setBaseUrl(next);
              touch("base_url");
            }}
            description="Any server with an OpenAI-style API: Azure OpenAI, vLLM, Ollama, OpenRouter, LiteLLM."
          />
        ) : null}
        <TextField
          id="connection-model"
          label="Default model"
          value={defaultModel}
          list={suggestions.length > 0 ? "connection-model-suggestions" : undefined}
          error={fieldError("default_model")}
          disabled={busy}
          onChange={(next) => {
            setDefaultModel(next);
            touch("default_model");
          }}
        />
        {suggestions.length > 0 ? (
          <datalist id="connection-model-suggestions">
            {suggestions.map((name) => (
              <option key={name} value={name} />
            ))}
          </datalist>
        ) : null}
        <details
          open={advancedOpen}
          onToggle={(event) => {
            setAdvancedOpen(event.currentTarget.open);
          }}
        >
          <summary className="cursor-pointer text-body font-medium">Advanced</summary>
          <div className="mt-4 flex flex-col gap-4">
            {provider !== "openai_compatible" ? (
              <TextField
                id="connection-base-url"
                label="Base URL"
                placeholder={DEFAULT_BASE_URLS[adapter]}
                value={baseUrl}
                error={fieldError("base_url")}
                disabled={busy}
                onChange={(next) => {
                  setBaseUrl(next);
                  touch("base_url");
                }}
                description="Only for a proxy. Leave empty to use the provider's API."
              />
            ) : null}
            <div className="flex flex-col gap-2">
              <Label htmlFor="connection-structured-output">Structured output</Label>
              <Select
                value={structured}
                onValueChange={(value) => {
                  setStructured(structuredValue(value));
                  touch("structured_output");
                }}
                disabled={busy}
              >
                <SelectTrigger
                  id="connection-structured-output"
                  aria-describedby="connection-structured-output-description"
                  className="w-fit"
                >
                  <SelectValue placeholder="Native (default)" />
                </SelectTrigger>
                <SelectContent>
                  {STRUCTURED_OUTPUT_MODES.map((option) => (
                    <SelectItem key={option.value} value={option.value}>
                      {option.label}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
              <p
                id="connection-structured-output-description"
                className="text-caption text-muted-foreground"
              >
                Use Tool call or Prompt only if the provider rejects native structured output.
              </p>
            </div>
            <div className="flex flex-col gap-4 sm:flex-row">
              <TextField
                id="connection-max-concurrency"
                type="number"
                label="Max concurrency"
                value={maxConcurrency}
                placeholder="4"
                unit="concurrent"
                inputMode="numeric"
                min={1}
                max={64}
                error={fieldError("max_concurrency")}
                disabled={busy}
                onChange={(next) => {
                  setMaxConcurrency(next);
                  touch("max_concurrency");
                }}
              />
              <TextField
                id="connection-requests-per-minute"
                type="number"
                label="Requests per minute"
                value={requestsPerMinute}
                placeholder="300"
                unit="per minute"
                inputMode="numeric"
                min={1}
                max={10000}
                error={fieldError("requests_per_minute")}
                disabled={busy}
                onChange={(next) => {
                  setRequestsPerMinute(next);
                  touch("requests_per_minute");
                }}
              />
            </div>
            <fieldset className="flex flex-col gap-2">
              <legend className="text-body leading-none font-medium">Extra headers</legend>
              <div className="flex items-center justify-end gap-2">
                {connection !== undefined ? (
                  <p className="mr-auto text-caption text-muted-foreground">
                    Values are hidden. Set every header again to change them.
                  </p>
                ) : null}
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  disabled={busy}
                  onClick={addHeaderRow}
                >
                  Add header
                </Button>
              </div>
              {rows.map((row, index) => (
                <div key={row.id} className="flex items-center gap-2">
                  <Input
                    aria-label={`Header name ${index + 1}`}
                    aria-invalid={headersError ? true : undefined}
                    value={row.name}
                    disabled={busy}
                    onChange={(event) => updateHeaderRow(row.id, { name: event.target.value })}
                  />
                  <Input
                    aria-label={`Header value ${index + 1}`}
                    aria-invalid={headersError ? true : undefined}
                    type="password"
                    autoComplete="off"
                    value={row.value}
                    disabled={busy}
                    onChange={(event) => updateHeaderRow(row.id, { value: event.target.value })}
                  />
                  <Button
                    type="button"
                    variant="ghost"
                    size="icon-xs"
                    aria-label={`Remove header ${index + 1}`}
                    disabled={busy}
                    onClick={() => removeHeaderRow(row.id)}
                  >
                    <X />
                  </Button>
                </div>
              ))}
              {headersError ? (
                <p role="alert" className="text-caption text-destructive">
                  {headersError}
                </p>
              ) : null}
            </fieldset>
            <TextField
              id="connection-embedding-model"
              label="Embedding model"
              value={embeddingModel}
              disabled={busy || adapter === "anthropic"}
              description="Optional. Checks a model that issue sources embed with."
              onChange={(next) => {
                setEmbeddingModel(next);
                touch("embedding_model");
              }}
            />
            <DisabledReason reason={embeddingTestReason}>
              <Button
                type="button"
                variant="outline"
                size="sm"
                disabled={embeddingTestReason !== undefined || testing || pending}
                onClick={() => runTest("embedding")}
              >
                Test embeddings
              </Button>
            </DisabledReason>
          </div>
        </details>
      </div>
      <div ref={testRegion} tabIndex={-1} className="max-w-xl focus:outline-none">
        <div aria-live="polite">
          {testResult ? <ConnectionTestResult result={testResult} /> : null}
        </div>
        <FormError message={testError} />
      </div>
    </SettingsFormCard>
  );
}
