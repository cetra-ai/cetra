import { AlignLeft, Play, Square } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { $api } from "@/api/client";
import { problemOf } from "@/api/problem";
import type { Schema } from "@/api/types";
import { useProject } from "@/components/shell/project-context";
import { ExamplesMenu } from "@/components/sql/examples-menu";
import { ExportMenu } from "@/components/sql/export-menu";
import { HistoryMenu } from "@/components/sql/history-menu";
import { ParamsBar } from "@/components/sql/params-bar";
import { SqlResultChart } from "@/components/sql/result-chart";
import { SqlResultGrid } from "@/components/sql/result-grid";
import { ResultsEmpty } from "@/components/sql/results-empty";
import { SavedQueriesMenu } from "@/components/sql/saved-queries-menu";
import { SqlEditor } from "@/components/sql/sql-editor";
import { Button } from "@/components/ui/button";
import { ResizableHandle, ResizablePanel, ResizablePanelGroup } from "@/components/ui/resizable";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { atLeast } from "@/lib/roles";
import type { SqlSearch } from "@/lib/search/sql";
import type { DraftUpdate, SqlDraft } from "@/lib/sql/draft";
import type { SqlExample } from "@/lib/sql/examples";
import { downloadSqlExport, type ExportFormat } from "@/lib/sql/export";
import {
  formatShortcutLabel,
  isSqlResult,
  RUN_FAILED,
  resultStatus,
  runShortcutLabel,
  sqlProblemMessage,
} from "@/lib/sql/format";
import { formatSql } from "@/lib/sql/format-sql";
import { pushHistory } from "@/lib/sql/history";
import { paramValues, sqlParams, usedParams } from "@/lib/sql/params";
import { sqlErrorRange } from "@/lib/sql/position";
import { sqlRunBody } from "@/lib/sql/request";
import { savedStateOf } from "@/lib/sql/saved-state";
import { DEFAULT_SQL_SPLIT, readSqlSplit, type SqlSplit, writeSqlSplit } from "@/lib/sql/split";
import type { SqlTab } from "@/lib/sql/tabs";
import { type RangeState, resolveRange } from "@/lib/time-range";

type LastRun = { result: Schema<"SqlQueryResult">; durationMs: number };

/**
 * One query tab: its editor, parameters, run and export, and its result. Every tab keeps its
 * own body mounted, so switching back shows the editor text and the result as they were.
 */
export function SqlTabBody({
  tab,
  active,
  schema,
  functionNames,
  range,
  search,
  insert,
  updateDraft,
  onLoadView,
  onSavedView,
  onClearedView,
  onRunningChange,
}: {
  tab: SqlTab;
  /** Only the active tab is shown; the others stay mounted. */
  active: boolean;
  schema: Schema<"SchemaResponse"> | undefined;
  /** The functions the console allows, for Format. */
  functionNames: readonly string[];
  range: RangeState;
  /** The URL search: the range is stored with a saved query. */
  search: SqlSearch;
  /** Each `seq` greater than the last one inserts `text` into this editor. */
  insert: { text: string; seq: number } | undefined;
  updateDraft: DraftUpdate;
  /** Reloads a saved query into this tab ("Discard changes"). */
  onLoadView: (view: Schema<"SavedView">) => void;
  /** This tab's draft was saved as a new query. */
  onSavedView: (view: Schema<"SavedView">) => void;
  /** This tab's saved query was deleted. */
  onClearedView: () => void;
  /** The tab's query starts or stops running. */
  onRunningChange: (tabId: string, running: boolean) => void;
}) {
  const { ref, data } = useProject();
  const draft: SqlDraft = tab.draft;
  const errorId = `sql-error-${tab.id}`;
  const run = $api.useMutation("post", "/api/v1/projects/{project}/sql", { gcTime: 0 });
  const [lastRun, setLastRun] = useState<LastRun | undefined>();
  /** The last run "succeeded" without a result (an empty error body). */
  const [badAnswer, setBadAnswer] = useState(false);
  const [exporting, setExporting] = useState(false);
  const [exportError, setExportError] = useState<string | undefined>();
  const [resultView, setResultView] = useState<"table" | "chart">("table");
  /**
   * True from the moment a run or export starts until it settles, without waiting for a
   * re-render: two fast Mod-Enter presses before `busy` updates must start one request.
   */
  const inFlight = useRef(false);
  /** Aborts the running request; created per run. */
  const controller = useRef<AbortController | undefined>(undefined);
  /** The user cancelled the last run: its aborted error is shown as a status, not a failure. */
  const [cancelled, setCancelled] = useState(false);
  const [split] = useState<SqlSplit>(() => readSqlSplit() ?? DEFAULT_SQL_SPLIT);
  const busy = run.isPending || exporting;
  const params = useMemo(() => sqlParams(draft.query), [draft.query]);
  const [paramErrors, setParamErrors] = useState<Record<string, string>>({});
  const pending = run.isPending;

  useEffect(() => {
    onRunningChange(tab.id, pending);
    return () => onRunningChange(tab.id, false);
  }, [tab.id, pending, onRunningChange]);

  // Closing the tab drops its request: the server stops the query when the request is dropped.
  useEffect(
    () => () => {
      controller.current?.abort();
    },
    [],
  );

  /** Loads an example the way the Examples menu does: its text, no parameters. */
  function pickExample(example: SqlExample) {
    setParamErrors({});
    updateDraft((previous) => ({ ...previous, query: example.query, params: {} }));
  }

  /** The request for `text`, or undefined when a parameter is invalid (errors are shown). */
  function buildBody(text: string): Schema<"SqlQueryRequest"> | undefined {
    const typed = paramValues(sqlParams(text), draft.params);
    if (Object.keys(typed.errors).length > 0) {
      setParamErrors(typed.errors);
      return undefined;
    }
    setParamErrors({});
    const span = resolveRange(range, Date.now());
    return sqlRunBody({ query: text, from: span.from, to: span.to, params: typed.values });
  }

  function execute(text: string) {
    if (text.trim().length === 0 || busy || inFlight.current) {
      return;
    }
    setExportError(undefined);
    const body = buildBody(text);
    if (!body) {
      return;
    }
    const used = usedParams(text, draft.params);
    const started = performance.now();
    inFlight.current = true;
    setLastRun(undefined);
    setBadAnswer(false);
    setCancelled(false);
    const abort = new AbortController();
    controller.current = abort;
    run.mutate(
      {
        params: { path: { project: ref } },
        body,
        headers: { Accept: "application/json" },
        signal: abort.signal,
      },
      {
        onSuccess: (data) => {
          const ranAt = new Date().toISOString();
          if (!isSqlResult(data)) {
            setBadAnswer(true);
            pushHistory(ref, { query: text, params: used, ranAt });
            return;
          }
          setLastRun({ result: data, durationMs: Math.round(performance.now() - started) });
          pushHistory(ref, { query: text, params: used, ranAt, rows: data.rows.length });
        },
        onError: () => {
          pushHistory(ref, { query: text, params: used, ranAt: new Date().toISOString() });
        },
        onSettled: () => {
          inFlight.current = false;
        },
      },
    );
  }

  /** Drops the running request. The server stops the query when the request is dropped. */
  function cancel() {
    controller.current?.abort();
    setCancelled(true);
  }

  /** Sends the same body as Run with the format's `Accept` and saves the file. */
  async function exportAs(format: ExportFormat) {
    if (draft.query.trim().length === 0 || busy || inFlight.current) {
      return;
    }
    const body = buildBody(draft.query);
    if (!body) {
      return;
    }
    inFlight.current = true;
    setExporting(true);
    setExportError(undefined);
    const outcome = await downloadSqlExport({ projectRef: ref, body, format, role: data.role });
    inFlight.current = false;
    setExporting(false);
    if (!outcome.ok) {
      setExportError(outcome.message);
    }
  }

  /** Replaces the query with its formatted text. Unparseable text comes back unchanged. */
  function formatQuery() {
    updateDraft((previous) => ({ ...previous, query: formatSql(previous.query, functionNames) }));
  }

  /** A cancelled run is not a failure: no error, and the editor is not marked invalid. */
  const failed = run.isError && !cancelled;
  const problem = failed ? problemOf(run.error) : undefined;
  const ranQuery = run.variables?.body.query;
  const position = problem?.errors?.find((error) => error.position)?.position;
  const failedHere = (failed || badAnswer) && ranQuery === draft.query;
  const errorRange = position && failedHere ? sqlErrorRange(draft.query, position) : null;
  const failure = failed
    ? problem
      ? sqlProblemMessage(problem)
      : RUN_FAILED
    : badAnswer
      ? RUN_FAILED
      : undefined;

  return (
    <div hidden={!active} className="flex min-h-0 flex-1 flex-col">
      <div className="flex min-h-9 shrink-0 items-center gap-2 overflow-x-auto border-b px-3">
        <SavedQueriesMenu
          viewId={draft.view}
          current={savedStateOf(draft, search)}
          onLoad={onLoadView}
          onSaved={onSavedView}
          onCleared={onClearedView}
        />
        <ExamplesMenu onPick={pickExample} />
        <HistoryMenu
          projectRef={ref}
          onPick={(entry) => {
            setParamErrors({});
            updateDraft((previous) => ({
              ...previous,
              query: entry.query,
              params: entry.params,
            }));
          }}
        />
        <Button
          type="button"
          variant="outline"
          size="xs"
          title={`Format (${formatShortcutLabel(navigator.userAgent)})`}
          disabled={draft.query.trim().length === 0}
          onClick={formatQuery}
        >
          <AlignLeft />
          Format
        </Button>
        <ExportMenu
          canExport={atLeast(data.role, "member")}
          disabled={draft.query.trim().length === 0 || busy}
          onExport={(format) => void exportAs(format)}
        />
        <div className="ml-auto flex shrink-0 items-center gap-2">
          {run.isPending ? (
            <Button type="button" variant="outline" size="xs" onClick={cancel}>
              <Square />
              Cancel
            </Button>
          ) : (
            <Button
              type="button"
              size="xs"
              title={`Run (${runShortcutLabel(navigator.userAgent)})`}
              disabled={draft.query.trim().length === 0 || busy}
              onClick={() => execute(draft.query)}
            >
              <Play />
              Run
            </Button>
          )}
        </div>
      </div>
      <div className="flex min-h-0 flex-1 flex-col gap-2 p-3">
        <ParamsBar
          params={params}
          values={draft.params}
          errors={paramErrors}
          onChange={(name, value) => {
            updateDraft((previous) => ({
              ...previous,
              params: { ...previous.params, [name]: value },
            }));
            setParamErrors((previous) => {
              const { [name]: _cleared, ...rest } = previous;
              return rest;
            });
          }}
        />
        <ResizablePanelGroup
          id={`sql-split-${tab.id}`}
          orientation="vertical"
          className="min-h-0 flex-1"
          defaultLayout={split}
          onLayoutChanged={(_layout, meta) => {
            if (!meta.isUserInteraction || !meta.requestedLayout) {
              return;
            }
            const editor = meta.requestedLayout.editor;
            const results = meta.requestedLayout.results;
            if (editor === undefined || results === undefined) {
              return;
            }
            writeSqlSplit({ editor, results });
          }}
        >
          <ResizablePanel
            id="editor"
            minSize="15%"
            className="relative"
            style={{ overflow: "hidden" }}
          >
            <div className="absolute inset-0 flex min-h-0 flex-col pb-2">
              <div className="min-h-0 flex-1">
                <SqlEditor
                  value={draft.query}
                  onChange={(text) => {
                    updateDraft((previous) => ({ ...previous, query: text }));
                    const names = new Set(sqlParams(text).map((param) => param.name));
                    setParamErrors((previous) =>
                      Object.fromEntries(
                        Object.entries(previous).filter(([name]) => names.has(name)),
                      ),
                    );
                  }}
                  onRun={execute}
                  errorRange={errorRange}
                  schema={schema}
                  invalid={failedHere}
                  describedBy={failedHere ? errorId : undefined}
                  insert={insert}
                />
              </div>
            </div>
          </ResizablePanel>
          <ResizableHandle />
          <ResizablePanel
            id="results"
            minSize="20%"
            className="relative"
            style={{ overflow: "hidden" }}
          >
            <div className="absolute inset-0 flex min-h-0 flex-col gap-2 pt-2">
              {failure ? (
                <p id={errorId} role="alert" className="text-body text-destructive">
                  {failure}
                </p>
              ) : null}
              {exportError ? (
                <p role="alert" className="text-body text-destructive">
                  {exportError}
                </p>
              ) : null}
              {exporting ? (
                <p role="status" className="text-body text-muted-foreground">
                  Exporting…
                </p>
              ) : null}
              <div className="flex min-h-0 flex-1 flex-col gap-1">
                {run.isPending ? (
                  <p role="status" className="text-body text-muted-foreground">
                    Running…
                  </p>
                ) : cancelled ? (
                  <p role="status" className="text-body text-muted-foreground">
                    Query cancelled.
                  </p>
                ) : lastRun && !run.isError ? (
                  <>
                    <p role="status" className="text-body text-muted-foreground">
                      {resultStatus(
                        lastRun.result.rows.length,
                        lastRun.result.truncated,
                        lastRun.durationMs,
                      )}
                    </p>
                    <Tabs
                      value={resultView}
                      onValueChange={(value) =>
                        setResultView(value === "chart" ? "chart" : "table")
                      }
                      className="flex min-h-0 flex-1 flex-col"
                    >
                      <TabsList>
                        <TabsTrigger value="table">Table</TabsTrigger>
                        <TabsTrigger value="chart">Chart</TabsTrigger>
                      </TabsList>
                      <TabsContent value="table" className="flex min-h-0 flex-1 flex-col">
                        <SqlResultGrid result={lastRun.result} />
                      </TabsContent>
                      <TabsContent value="chart" className="flex min-h-0 flex-1 flex-col">
                        <SqlResultChart result={lastRun.result} />
                      </TabsContent>
                    </Tabs>
                  </>
                ) : run.isIdle ? (
                  <ResultsEmpty onPick={pickExample} />
                ) : null}
              </div>
            </div>
          </ResizablePanel>
        </ResizablePanelGroup>
      </div>
    </div>
  );
}
