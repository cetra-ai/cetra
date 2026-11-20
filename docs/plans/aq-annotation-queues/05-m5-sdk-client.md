# AQ M5 — SDK `Client` methods for annotation queues

**Summary.** This milestone adds annotation queues to the REST `Client` of both SDKs: list, get, create, update, delete, add items and list items, over M2's routes (D-AQ-8: "the REST API and the Python and TypeScript `Client`"). The reviewer actions are not exposed, because they need a person (D-AQ-11).

There are two phases, one per SDK, and they run in parallel: they share no file.

**Prerequisites.** M2's Q4 is committed. The routes, `openapi.json` and the fixtures `fixtures/api/static/annotation-queue*.json` exist. Work on branch `claude/aq-impl` in `/home/sebastien/travaux/llm-observer/.claude/worktrees/aq-annotation-queues-f38be7`.

**Read first.**
- `00-design.md` § Milestones (M5) and `02-m2-queues-server.md` § Q4, steps 3 and 4: the DTOs and the routes.
- `crates/server/openapi.json`, the `AnnotationQueue*`, `QueueCounts`, `AddQueueItems` and `AddedQueueItems` schemas.
- Python: `sdks/python/src/cetra/_client.py` (`Client`, `_request`, `_segment`), `sdks/python/src/cetra/_datasets.py` (the mirror), `sdks/python/src/cetra/__init__.py`, `sdks/python/tests/test_datasets.py`, `sdks/python/tests/conftest.py` (`http_recorder`), `sdks/python/tests/test_e2e.py`, and `sdks/python/tests/test_api_reference.py`.
- TypeScript: `sdks/typescript/src/client.ts`, `sdks/typescript/src/datasets.ts` (the mirror, including `collect` and `pageQuery`), `sdks/typescript/src/index.ts`, `sdks/typescript/src/datasets.test.ts`, `sdks/typescript/src/rest.test-helpers.ts`, `sdks/typescript/src/e2e.test.ts`, and `sdks/typescript/src/api-reference.test.ts`.
- `website/src/content/docs/docs/reference/{python,typescript}/api.mdx` and `…/scores-datasets-experiments.mdx`; `website/STYLE.md`.

## What it is, and what it is not

**What works when it's done:**

```python
client = cetra.Client()
queue = client.annotation_queues.create("Weekly review", target="trace", definition_ids=[d1, d2])
result = client.annotation_queues.add_items(queue.id, trace_ids=["4bf92f3577b34da6a3ce929d0e0e4736"])
items = client.annotation_queues.items(queue.id, status="pending")
```

```ts
const queue = await client.annotationQueues.create("Weekly review", { target: "trace", definitionIds: [d1, d2] });
const result = await client.annotationQueues.addItems(queue.id, { traceIds: ["4bf92f…"] });
```

**Out of scope:**
- `next`, lease, complete, skip and reopen (a person's actions);
- adds by filter (M3's routes are not wrapped; the UI uses them);
- a score-definition client.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-AQ-8 | Adds 1–1 000 targets per call; `skipped` with indexes; re-adding keeps the state |
| D-AQ-11 | `sk` keys with `write` manage queues and items |
| D-J-16 | The dataset `Client` conventions: snake_case Python dataclasses and camelCase TS interfaces mapped from snake_case wire types; `list` collects every page |

Plan-level choices:
- **Batching.** `add_items` sends batches of 1 000. It sums `added` and `existing`, and shifts each `skipped[].index` by the batch's start, so indexes refer to the caller's list.
- **Retries.** `add_items` is idempotent, so it is sent with `retry=True`. `create`, `update` and `delete` follow the dataset methods: writes are sent once, deletes retried as the client does for DELETE.
- **Fixture bodies.** Unit tests answer with the committed fixtures `fixtures/api/static/annotation-queue*.json` read from disk, so the SDKs and the server agree on the shape. These are the first SDK tests to read `fixtures/api`.

## Versions

No new dependency.

## Phases

```
S1 (Python) ∥ S2 (TypeScript)
```

Only S1 edits `docs/spec/10-sdk.md` and `CHANGELOG.md`. S2 edits only TypeScript files and the TypeScript reference pages.

---

### S1 — Python

**Goal.** `client.annotation_queues` with typed results, tests, e2e, and docs.

**Edit:**
- `sdks/python/src/cetra/_annotation_queues.py` (new)
- `sdks/python/src/cetra/_client.py`
- `sdks/python/src/cetra/__init__.py`
- `sdks/python/tests/test_annotation_queues.py` (new)
- `sdks/python/tests/test_e2e.py`
- `website/src/content/docs/docs/reference/python/api.mdx`
- `website/src/content/docs/docs/reference/python/scores-datasets-experiments.mdx`
- `docs/spec/10-sdk.md`
- `CHANGELOG.md`

**Steps.**
1. Create `_annotation_queues.py`. Mirror `_datasets.py` for the imports, the module docstring and the mapping helpers. Import `Literal` from `typing` and `Sequence` from `collections.abc` (ruff UP035). Its public types:
   ```python
   QueueTarget = Literal["trace", "span", "session"]
   QueueItemStatus = Literal["pending", "completed", "skipped"]

   @dataclass(frozen=True, slots=True)
   class QueueCounts:
       pending: int
       completed: int
       skipped: int

   @dataclass(frozen=True, slots=True)
   class AnnotationQueue:
       id: str
       name: str
       description: str | None
       instructions: str | None
       target: QueueTarget
       definition_ids: tuple[str, ...]
       version: int
       created_by: str | None
       created_at: str          # RFC 3339, as the dataset types keep times
       updated_at: str
       counts: QueueCounts

   @dataclass(frozen=True, slots=True)
   class AnnotationQueueItem:
       id: str
       queue_id: str
       trace_id: str | None
       span_id: str | None
       session_id: str | None
       target_time: str
       status: QueueItemStatus
       skip_reason: str | None
       leased_by_user_id: str | None
       lease_expires_at: str | None
       done_by_user_id: str | None
       done_at: str | None
       added_by: Literal["user", "pat", "key"]
       added_by_id: str
       created_at: str

   @dataclass(frozen=True, slots=True)
   class SkippedQueueItem:
       index: int
       message: str

   @dataclass(frozen=True, slots=True)
   class AddedQueueItems:
       added: int
       existing: int
       skipped: tuple[SkippedQueueItem, ...]
   ```
   If `_datasets.py` keeps times as something other than `str`, use what it uses.
2. Write the class `AnnotationQueues` with `__init__(self, client: Client)`. Define the methods in this order, with `list` last, so it does not shadow `list[...]` in earlier annotations:
   - `get(self, queue_id: str) -> AnnotationQueue`: `GET /annotation-queues/{id}`.
   - `create(self, name: str, *, target: QueueTarget, definition_ids: Sequence[str], description: str | None = None, instructions: str | None = None) -> AnnotationQueue`: `POST /annotation-queues`. The body leaves out `None` fields.
   - `update(self, queue_id: str, *, version: int, name: str | None = None, description: str | None = None, instructions: str | None = None, definition_ids: Sequence[str] | None = None) -> AnnotationQueue`: `PATCH`. The body is `version` plus the given fields. `""` clears a text field, as the API does.
   - `delete(self, queue_id: str) -> None`: `DELETE`.
   - `add_items(self, queue_id: str, *, trace_ids: Sequence[str] | None = None, spans: Sequence[tuple[str, str]] | None = None, session_ids: Sequence[str] | None = None) -> AddedQueueItems`.
     - Exactly one of the three must be given, else `ValueError("give exactly one of trace_ids, spans or session_ids")`. A plain `str` passed as `trace_ids` or `session_ids` is a `TypeError` ("trace_ids must be a sequence of ids, not a string"), and a `spans` entry that is not a pair of two strings is a `TypeError` naming its index (never the values). `create` raises `ValueError` for a `target` outside the three kinds and for empty `definition_ids`. All of these are raised before any request.
     - An empty sequence returns `AddedQueueItems(0, 0, ())` without a request.
     - It sends `POST /annotation-queues/{id}/items` with `{"items": [{"trace_id": …} | {"trace_id": …, "span_id": …} | {"session_id": …}]}` in batches of 1 000, with `retry=True`.
     - It sums the counts and shifts the skipped indexes by the batch's start.
   - `items(self, queue_id: str, *, status: QueueItemStatus | None = None) -> list[AnnotationQueueItem]`: `GET /annotation-queues/{id}/items` with `limit=1000` and `status` when given. It follows `next_cursor` as `Datasets.list` does.
   - `list(self) -> list[AnnotationQueue]`: `GET /annotation-queues` with `limit=1000`, collecting every page. Pages end when `next_cursor` is null (the server always sends the key; read it with `.get("next_cursor")`).
   - Path ids go through `_segment`. The mapping uses `raw["x"]` for required keys and `raw.get("x")` for omitted optionals, as `_datasets.py` does.
3. In `_client.py`, build `self._annotation_queues = AnnotationQueues(self)` at the end of `Client.__init__`, with the deferred import the datasets use. Add the property:
   ```python
   @property
   def annotation_queues(self) -> AnnotationQueues:
       """Annotation queues of the key's project (D-AQ-8)."""
   ```
4. In `__init__.py`, add `AnnotationQueue`, `AnnotationQueueItem`, `AddedQueueItems`, `QueueCounts`, `QueueItemStatus`, `QueueTarget` and `SkippedQueueItem` to the imports and to `__all__`, in its order. Do not export the `AnnotationQueues` class: `Datasets` is not exported either.
5. In `reference/python/api.mdx`:
   - add one `### \`Name\`` entry per new export, in the style of the dataset type entries (the `{/* untested: signature */}` marker, a fence, and a short paragraph ending "See [Annotation queues](…)");
   - add `annotation_queues` to the `Client` entry, and update that entry's one-line wording if it says the client is for datasets and experiments only.
6. In `reference/python/scores-datasets-experiments.mdx`, add `### Annotation queues` after `### Datasets` (re-run `node website/scripts/check-terms.mjs` after writing prose; `website/STYLE.md` lists banned terms):
   - one sentence: queues are worked in the web UI, and the SDK creates them and fills them;
   - one sample under `{/* untested: needs a running cetra server and an API key */}`: create, add trace ids, read `result.skipped`;
   - one bullet list of the methods.
7. In `docs/spec/10-sdk.md` § Datasets and experiments (Python), add `### Annotation queues` with the snippet of § What it is and the rules:
   - batches of 1 000;
   - skipped indexes are positions in the caller's list;
   - the reviewer actions are web only (D-AQ-11).

   In the TypeScript section, add a bullet: "Annotation queues: `client.annotationQueues.*`, the same methods in camelCase."
8. In `CHANGELOG.md` § `[Unreleased]` § Added: "`Client.annotation_queues` (Python) and `client.annotationQueues` (TypeScript): create annotation queues and add traces, spans or sessions to them."

**Tests** in `tests/test_annotation_queues.py`. They use `http_recorder`; replies are read from `Path(__file__).parents[3] / "fixtures" / "api" / "static" / <file>`: `annotation-queues.json` for `list`, `annotation-queue.json` for `create` and `get`, `annotation-queue-items.json` for `items`, `annotation-queue-items-added.json` for `add_items`. If one is missing, stop and report. Assert the mapped values against the fixture ids (queue `…0a01`).
- `create_sends_the_body_and_maps_the_queue`: checks the method, the path `/api/v1/projects/current/annotation-queues`, and that the body has no `None` fields. The reply is `annotation-queue.json`; check the mapped fields, including `counts` and `definition_ids` as a tuple.
- `list_follows_cursors`: two queued pages (the first with `next_cursor`). The result is both pages' queues; the second request carries `cursor`.
- `get_update_delete`: check the paths, that the `PATCH` body has `version` and only the given fields, and that `delete` returns `None` on 204.
- `add_items_shapes`: `trace_ids`, `spans` and `session_ids` each send the matching item objects.
- `add_items_needs_exactly_one_kind`: none or two kinds raise `ValueError`.
- `add_items_batches_and_offsets_skipped`: 1 500 trace ids give two requests of 1 000 and 500. The second reply skips index 3; the result has index 1 003, and the summed counts.
- `add_items_empty_sends_nothing`.
- `items_filters_by_status`: the query has `status=pending&limit=1000`; the reply is `annotation-queue-items.json`.
- `errors_are_api_errors`: a 403 `plan_limit` problem raises `ApiError` with `code == "plan_limit"`. A 409 stale-version problem raises it with `status == 409`.
- In `test_e2e.py`, `test_annotation_queue_round_trip` (marker `e2e`):
  - create a score definition with a raw `requests.post(f"{base}/api/v1/projects/current/score-definitions", json={"name": f"e2e-py-{uuid7()}", "data_type": "boolean"}, headers={"Authorization": f"Bearer {key}"})`, where `base` and `key` are what the file's existing tests use; the answer's `id` is the definition id. Score definitions need `ManageEvaluation`, which API keys cannot use (D-I-24): if the e2e key is refused (`403`), create the definition through a session or PAT the way `test_e2e.py` already authenticates for any member-only call; if the file has no such path, skip the definition step, report it, and assert instead that `create` with a random definition id raises `ApiError` with `status == 400`. Leave the definition in place (the e2e server is throwaway);
  - create a trace queue;
  - `add_items` one random trace id: `added == 0`, and `skipped[0].index == 0`;
  - `items` is empty, and `list` contains the queue;
  - delete it in `finally`.

**Done when.**
- `cd sdks/python && uv run pytest tests/test_annotation_queues.py tests/test_api_reference.py -q`
- `cd sdks/python && uv run ruff format --check && uv run ruff check && uv run mypy`
- `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check-py && just check-docs`
- Do not run `just e2e` (it builds the server; the coordinator runs it at the end). The e2e test must still pass `ruff` and `mypy`.

**Commit:** `Add annotation queues to the Python client`

---

### S2 — TypeScript

**Goal.** `client.annotationQueues` with typed results, tests, e2e and docs.

**Edit:**
- `sdks/typescript/src/annotation-queues.ts` (new)
- `sdks/typescript/src/datasets.ts` (only: add `export` to `pageQuery` and `collect`)
- `sdks/typescript/src/client.ts`
- `sdks/typescript/src/index.ts`
- `sdks/typescript/src/annotation-queues.test.ts` (new)
- `sdks/typescript/src/e2e.test.ts`
- `website/src/content/docs/docs/reference/typescript/api.mdx`
- `website/src/content/docs/docs/reference/typescript/scores-datasets-experiments.mdx`

**Steps.**
1. Create `annotation-queues.ts`, mirroring `datasets.ts`: `readonly` camelCase interfaces, private snake_case `Wire*` interfaces and `…FromWire` mappers. Optional fields are `T | undefined`.
   ```ts
   export type QueueTarget = "trace" | "span" | "session";
   export type QueueItemStatus = "pending" | "completed" | "skipped";
   export interface QueueCounts { readonly pending: number; readonly completed: number; readonly skipped: number }
   export interface AnnotationQueue {
     readonly id: string; readonly name: string;
     readonly description: string | undefined; readonly instructions: string | undefined;
     readonly target: QueueTarget; readonly definitionIds: readonly string[];
     readonly version: number; readonly createdBy: string | undefined;
     readonly createdAt: string; readonly updatedAt: string; readonly counts: QueueCounts;
   }
   export interface AnnotationQueueItem {
     readonly id: string; readonly queueId: string;
     readonly traceId: string | undefined; readonly spanId: string | undefined; readonly sessionId: string | undefined;
     readonly targetTime: string; readonly status: QueueItemStatus; readonly skipReason: string | undefined;
     readonly leasedByUserId: string | undefined; readonly leaseExpiresAt: string | undefined;
     readonly doneByUserId: string | undefined; readonly doneAt: string | undefined;
     readonly addedBy: "user" | "pat" | "key"; readonly addedById: string; readonly createdAt: string;
   }
   export interface SkippedQueueItem { readonly index: number; readonly message: string }
   export interface AddedQueueItems { readonly added: number; readonly existing: number; readonly skipped: readonly SkippedQueueItem[] }
   export type QueueTargets =
     | { readonly traceIds: readonly string[] }
     | { readonly spans: readonly (readonly [string, string])[] }
     | { readonly sessionIds: readonly string[] };
   ```
   If `datasets.ts` keeps times as `Date` rather than `string`, follow it.
2. Write the class `AnnotationQueues`. Its constructor matches `Datasets`'s. Methods:
   - `list(): Promise<AnnotationQueue[]>`
   - `get(queueId: string): Promise<AnnotationQueue>`
   - `create(name: string, options: { target: QueueTarget; definitionIds: readonly string[]; description?: string; instructions?: string }): Promise<AnnotationQueue>`
   - `update(queueId: string, options: { version: number; name?: string; description?: string; instructions?: string; definitionIds?: readonly string[] }): Promise<AnnotationQueue>`
   - `delete(queueId: string): Promise<void>`
   - `addItems(queueId: string, targets: QueueTargets): Promise<AddedQueueItems>`
   - `items(queueId: string, options?: { status?: QueueItemStatus }): Promise<AnnotationQueueItem[]>`

   The behaviour is S1's, rules included:
   - `addItems` with an object holding more or fewer than one of the three keys throws `TypeError("give exactly one of traceIds, spans or sessionIds")`;
   - empty input sends nothing;
   - batches of 1 000, sent with `retry: true`, and the skipped indexes shifted.

   To page, add `export` to `collect` and `pageQuery` in `datasets.ts` (module-internal: not added to `index.ts`) and import them; read their signatures there and pass the `status` filter through `pageQuery`'s fields object. Do not copy them. Request bodies omit keys whose value is `undefined` (build them with conditional spreads), so `FakeRest` body assertions see no `undefined` keys.
3. In `client.ts`, add `readonly annotationQueues: AnnotationQueues`, built in the constructor beside `datasets`.
4. In `index.ts`, export `type AnnotationQueue`, `type AnnotationQueueItem`, `type AddedQueueItems`, `type QueueCounts`, `type QueueItemStatus`, `type QueueTarget`, `type QueueTargets`, `type SkippedQueueItem` and `type AnnotationQueues`, in the style of the dataset exports.
5. In `reference/typescript/api.mdx`, add one heading per new export, in the style of the dataset entries, and add `annotationQueues` to the `Client` entry. In `reference/typescript/scores-datasets-experiments.mdx`, add `### Annotation queues`, mirroring S1's Python section in TypeScript, with the same untested marker.

**Tests** in `annotation-queues.test.ts`. They use `FakeRest`, `inOrder` and `BASE`, plus a local `clientFor`. Fixture replies come from `readFileSync(new URL("../../../fixtures/api/static/<file>", import.meta.url), "utf8")`. Write the same cases as S1, named in camelCase sentences:
- `createSendsTheBodyAndMapsTheQueue`
- `listFollowsCursors`
- `getUpdateDelete`
- `addItemsShapes`
- `addItemsNeedsExactlyOneKind`
- `addItemsBatchesAndOffsetsSkipped`
- `addItemsEmptySendsNothing`
- `itemsFiltersByStatus`
- `errorsAreApiErrors`

In `e2e.test.ts`, add the same round trip as S1, reusing the file's single `init()`. Create the score definition with `fetch` and the same literal body and `Authorization` header, with S1's fallback if the key is refused.

**Done when.**
- `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`
- `pnpm --dir sdks/typescript exec vitest run src/annotation-queues.test.ts src/api-reference.test.ts`
- `pnpm --dir sdks/typescript exec biome check .` and `pnpm --dir sdks/typescript run typecheck`
- `just check-ts && just check-docs`
- Do not run `just e2e`.

**Commit:** `Add annotation queues to the TypeScript client`
