# Audio player and media transcripts (D-C-29)

Media parts (`uri` and `blob`) gain an optional `transcript`: the spoken text of an audio part, when known. Normalization keeps it through media extraction and truncation. OpenAI chat output audio (`message.audio = {data, transcript, …}`, ignored today) becomes an audio part carrying its transcript. A message made only of audio previews its transcripts. Judge prompts render `[audio: <transcript>]`. In the trace view's Chat tab, audio plays in a themed player with a waveform, and the transcript shows under it in the same block.

**Prerequisites:** none. Branch `claude/audio-player`, cut from `origin/main`.

**Read first:**
- `AGENTS.md` (rules, commands, privacy, no compatibility layers).
- `docs/spec/01-data-model.md` § Canonical message format (lines ~115–134) and the `input_preview` row (line ~85).
- `docs/spec/02-ingestion.md` § Media extraction and § Truncation.
- `docs/spec/11-ui.md` § Trace view, the *Chat* tab bullet.
- `crates/core/src/message.rs`: `Part`, `part_from_value`, `uri_part`, `blob_part`, `part_to_value`, `leftovers`, `optional_string`, `push_string`, and the `UriPart` / `BlobPart` OpenAPI schemas (`known_part`).
- `crates/otel/src/media.rs` `extract_part`, `uri_part`; `crates/otel/src/truncate.rs` `LeafPos`, `longest_leaf`, `leaf_text`, `set_leaf`; `crates/otel/src/payload.rs` `message_preview`; `crates/otel/src/convert/openai.rs` `message`; `crates/eval/src/text.rs` `render_part`, `media_placeholder`.
- `web/src/components/payload/media-view.tsx`, `part-view.tsx`, `message-view.tsx`, `media-view.test.tsx`.
- `fixtures/otlp/README.md` (hand-written case layout).

## What it is, and what it is not

When it's done:
- `{"type":"blob"|"uri", …, "transcript": "…"}` round-trips; `"transcript": null` is preserved like `mime_type`; a non-string transcript makes the part `Unknown` (kept verbatim), like any wrongly typed modelled field.
- Media extraction keeps the transcript on the `uri` part it writes, both when the blob is stored and when it is dropped as too large.
- Truncation can cut a transcript like a text leaf.
- `input_preview` / `output_preview` of a message with no text part are its media transcripts.
- OpenAI chat output audio is an audio `blob` part with its transcript.
- Judges see `[audio: <transcript>]`.
- The Chat tab plays audio in a themed player (play/pause, waveform with progress, elapsed and total time, MIME type and length), with the transcript under it.

Out of scope (not planned):
- Gemini, Vercel, OpenAI Realtime and Responses, LiveKit and Pipecat transcripts. Gemini and Vercel send no transcript on the part, Realtime is not an OTel span shape, and the voice frameworks send text-only STT/TTS spans with no audio.
- A transcript for the `{id}`-only OpenAI audio reference: an `audio` object without a string `data` stays in the message's `extra` as today.
- Searching transcripts beyond the preview.
- The website's showcase data (`web/src/mocks/generated/dataset.ts`), owned by the website session.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-C-29 (new, AP4 writes it) | `uri` and `blob` parts have an optional `transcript`. It is filled from cetra-format parts and OpenAI chat output audio. A text-less message previews its transcripts. Eval renders `[<media>: <transcript>]`. The Chat tab plays audio in a themed player with a waveform that is decoded when the player is visible, for files up to 4 MiB, and shows the transcript under it. |
| D65 | Nullable optional fields are preserved as explicit nulls (the OpenAPI nullability list). |
| D103 | Normalization returns blobs and the server stores them. |

The user's answers (2026-11-25):
- Transcript pairing: a data-model field, not adjacent text parts.
- Player: a waveform, not just a progress bar.
- Preview: transcripts only when the message has no text.
- Eval: `[audio: transcript]`.
- Sources: cetra format plus OpenAI chat output audio.

## Versions

No new dependency. The waveform uses the browser's Web Audio API (`OfflineAudioContext.decodeAudioData`).

## Phases

```
AP1 (core field, serde, OpenAPI, constructors, media carry-over)
 ├── AP2 (otel: preview, truncation, OpenAI output audio; eval text)   Rust only
 └── AP3 (web player and transcript)                                    web only
AP4 (docs, decision, changelog)   after AP2 and AP3
```

AP2 and AP3 can run in parallel after AP1: AP2 edits only `crates/`, `fixtures/otlp/` and `docs/spec/01`, `02` and `06`; AP3 edits only `web/src/components/payload/`.

---

## AP1 — The `transcript` field

**Goal:** `Part::Uri` and `Part::Blob` carry `transcript: Option<String>` end to end in Rust, and the API schema and the web's generated types have it.

**Edit:**
- `crates/core/src/message.rs`
- `crates/core/src/schema_drift.rs` (constructors only)
- `crates/core/tests/snapshots/part_schema.snap` (if the snapshot test rewrites it, accept the new snapshot)
- `crates/otel/src/media.rs`
- `crates/otel/src/convert/openai.rs`, `gemini.rs`, `vercel.rs`, `anthropic.rs` (constructors only: `transcript: None`)
- `crates/eval/src/chat/mod.rs`, `crates/eval/tests/chat_stream.rs` (test constructors only)
- `crates/server/tests/openapi.rs` (nullability list)
- `crates/server/openapi.json`, `web/src/api/schema.gen.ts` (regenerated by `just openapi`, never by hand)
- `docs/spec/01-data-model.md`
- Any other file the compiler names for a missing `transcript` field in a `Part::Uri` / `Part::Blob` constructor or pattern. Add `transcript: None` there and nothing else, and list each file in the report.

**Steps:**
1. In `Part::Uri` and `Part::Blob`, add after `mime_type`:
   ```rust
   /// Spoken text of the media (an audio part's transcript), when known.
   transcript: Option<String>,
   ```
2. `uri_part` / `blob_part` (parse): `transcript: optional_string(map, "transcript")?`, and add `"transcript"` to the optional slice of `leftovers` for both, e.g. `leftovers(map, &["type", "uri"], &["modality", "mime_type", "transcript"])`. An explicit null then stays in `extra` (as `mime_type` does today), and a non-string value fails the part, which becomes `Part::Unknown`.
3. `part_to_value` (write): for both variants, `push_string(&mut fields, "transcript", transcript);` right after `mime_type` and before `uri` / `content`. The key order of a written part is `type, modality, mime_type, transcript, uri|content`, then `extra`.
4. OpenAPI: in both `UriPart` and `BlobPart`, add after `mime_type`:
   ```rust
   (
       "transcript",
       nullable_string("Spoken text of the media (an audio part's transcript), when known."),
       false,
   ),
   ```
   In `crates/server/tests/openapi.rs`, add `"transcript"` to the list of fields that must be nullable (around the D65 assertion, ~line 2066; read the test to find the exact list).
5. Every other constructor of `Part::Uri` / `Part::Blob` gets `transcript: None`. Patterns that destructure with `..` need no change.
6. `crates/otel/src/media.rs`:
   - `extract_part`, `Part::Blob` arm: also bind `transcript` and pass `std::mem::take(transcript)` to `uri_part`. `Part::Uri` arm with an entire data URI: same.
   - `uri_part(mut extra, transcript: Option<String>, replacement)`: set `transcript` on the `Part::Uri` built in both the `Extracted` and the `Dropped` branches.
7. Run `just openapi` to regenerate `crates/server/openapi.json` and `web/src/api/schema.gen.ts`.
8. Spec 01 § Canonical message format:
   - In the example, change the audio-free example lines to add one audio line: `{"type": "uri", "modality": "audio", "mime_type": "audio/wav", "uri": "media://…", "transcript": "What's my balance?"}`.
   - In the explicit-null sentence, add `transcript` to the list: (`name`, `finish_reason`, `id`, `modality`, `mime_type`, `transcript`).
   - Add after that paragraph: "A `uri` or `blob` part may carry `transcript`, the spoken text of the media (for audio, what was said). Media extraction keeps it on the `uri` part it writes, including a dropped one."

**Tests** (in the `#[cfg(test)]` modules of the files edited):
- `message.rs`:
  - `a_blob_transcript_round_trips`: parse `{"type":"blob","modality":"audio","mime_type":"audio/wav","transcript":"hi","content":"QQ=="}` (writer order, step 3), get `Part::Blob { transcript: Some("hi"), extra empty }`, serialize back to the same JSON string.
  - `a_uri_transcript_round_trips`: same for `uri`.
  - `a_null_transcript_is_preserved`: `"transcript": null` parses to `transcript: None` with `extra["transcript"] == null`, and serializes back with `"transcript":null`.
  - `a_non_string_transcript_makes_the_part_unknown`: `"transcript": 3` → `Part::Unknown` equal to the input value.
  - `a_part_without_transcript_writes_no_key`: a blob without the key serializes without `transcript`.
- `media.rs`:
  - `an_extracted_blob_keeps_its_transcript`: a blob ≥ `min_bytes` with a transcript becomes a `media://` uri part with the same transcript.
  - `a_dropped_blob_keeps_its_transcript`: a blob over `max_bytes` becomes `cetra:dropped` with `reason`, `size` and the transcript.
  - `an_extracted_data_uri_keeps_its_transcript`: a `uri` part whose uri is an entire data URI, with a transcript.
  - `a_canonical_blob_inside_raw_json_keeps_its_transcript`: the raw-JSON path (`json_part` / `replaced_object`, ~line 262) already copies unknown keys, so it needs no code change; this test pins it (a `json` payload holding a canonical blob object with a transcript).
- The OpenAPI snapshot (`crates/core/tests/snapshots/part_schema.snap`, asserted at `message.rs` ~line 1073) is updated with `INSTA_UPDATE=always cargo test -p cetra-core`; read the diff: only the two `transcript` properties are new. `crates/server/tests/openapi.rs` passes with the new field.

**Done when** (from the worktree root):
- `cargo test -p cetra-core -p cetra-otel -p cetra-eval` passes (after the snapshot update above).
- `cargo test -p cetra-server --test openapi --test api_examples` passes (check the test target names in `crates/server/Cargo.toml` / `crates/server/tests/`; run the targets that match `Part::Uri`).
- `just openapi` needs the JS deps for the TypeScript generation: run `just js-install` first if `node_modules/` is missing.
- `cargo clippy -p cetra-core -p cetra-otel -p cetra-eval -p cetra-server --all-targets -- -D warnings` is clean, and `cargo fmt --check` is clean.
- `git diff --stat` shows `web/src/api/schema.gen.ts` with `transcript` on `UriPart` and `BlobPart`.
- Golden tests: `cargo test -p cetra-otel --test golden` passes with no `expected.json` change (an absent transcript writes no key).

**Commit:** `Add a transcript to media parts`

---

## AP2 — Transcripts in normalization and eval

**Goal:** previews, truncation, OpenAI chat output audio and judge text use the transcript.

**Edit:**
- `crates/otel/src/payload.rs`
- `crates/otel/src/truncate.rs`
- `crates/otel/src/convert/openai.rs`
- `crates/eval/src/text.rs`
- `fixtures/otlp/edge/openai-output-audio/{case.json,input.json,expected.json}` (new)
- `docs/spec/01-data-model.md` (the `input_preview` row only)
- `docs/spec/02-ingestion.md`
- `docs/spec/06-evaluation.md` (§ Defaults text extraction, § Rendering)

**Steps:**
1. **Preview** (`payload.rs` `message_preview`): collect the text parts as today. If there are none, collect instead the `transcript` of every `Part::Uri` / `Part::Blob` whose transcript is `Some` and not empty, in part order. Join with `"\n"` and cut with `cut_preview` as today; `None` when both lists are empty. Rename the test `message_previews_use_text_parts_only` to `message_previews_use_text_parts_first` and keep its assertions.
2. **Truncation** (`truncate.rs`):
   - Add `LeafPos::Transcript { message: usize, part: usize }`.
   - `longest_leaf`: a `Part::Uri { transcript: Some(text), .. }` or `Part::Blob { transcript: Some(text), .. }` with `leaf_open(text)` is a candidate `LeafPos::Transcript`.
   - `leaf_text` and `set_leaf`: read and write that transcript. `message_index` / `part_index` (or whatever maps a `LeafPos` to its indices) handle the new variant.
3. **OpenAI chat output audio** (`convert/openai.rs` `message`): after the `content` block and before `tool_calls`:
   - If `extra["audio"]` is a JSON object whose `data` is a string, remove `audio` from `extra` and push
     `Part::Blob { modality: Some(Modality::Audio), mime_type: None, content: data, transcript, extra }`, where:
     - `transcript` is `audio["transcript"]` when it is a string; a non-string `transcript` stays in the part's `extra`;
     - the part's `extra` is the audio object without `data` (and without `transcript` when it was taken), so `id` and `expires_at` are kept.
   - Otherwise (no `audio`, not an object, or no string `data`) leave `extra["audio"]` untouched.
   - `mime_type` stays `None`: the response does not say the format. Media extraction sniffs WAV, MP3, OGG and FLAC (spec 02 § Media extraction step 3). Raw PCM stays `application/octet-stream`.
4. **Eval** (`eval/src/text.rs`):
   - `last_assistant_text`: when the last assistant message has no text part, return its media transcripts (non-empty, part order) joined with `"\n"`, the same rule as the preview. A message with text keeps returning only its text.
   - A dropped part has no modality (spec 02: `cetra:dropped` parts are written without one), so a dropped audio renders `[file: <transcript>]`. That is intended; do not change the dropped shape.
   - Update the module doc (~lines 39–40, which list `[image]`, `[audio]`, `[file]`) with the transcript form.
   - `render_part` for `Part::Uri` / `Part::Blob` passes the transcript to `media_placeholder(modality, transcript)`, which returns `[audio]` etc. as today when the transcript is `None` or empty, and `[audio: <transcript>]` (same label, `": "`, the transcript verbatim) otherwise. Update the doc comment.
5. **Fixture** `fixtures/otlp/edge/openai-output-audio/`, hand-written (`origin: "hand_written"`), following `fixtures/otlp/README.md`:
   - One GenAI span whose `gen_ai.output.messages` attribute is a JSON string of OpenAI response choices (the Laminar shape that `conventions/genai.rs` `genai_messages` sends to `openai_messages`, D-B1-3): `[{"message":{"role":"assistant","content":null,"audio":{"id":"audio_1","data":"<base64>","transcript":"Your balance is 42 dollars.","expires_at":1767225600}},"finish_reason":"stop"}]`. Copy the span's other attributes (operation name, span kind) from an existing `fixtures/otlp/genai/` case that sets `gen_ai.output.messages`, with no usage attributes and no input messages, so the case needs no `prices.json` row and has one scenario.
   - `case.json` `settings`: `{"media_min_bytes": 64}`, so a small blob is extracted.
   - The `data` is a WAV of about 250 bytes: a 44-byte RIFF/WAVE header for 8 kHz, mono, 8-bit PCM, then 200 samples of silence (value 128). Generate the base64 with a one-off script in the scratchpad, not a file in the repo.
   - `spec_refs`: `["02 § Media extraction", "02 § Normalization rules", "01 § Canonical message format"]`.
   - Write `expected.json` with `CETRA_BLESS=1 cargo test -p cetra-otel --test golden`, then `just fixtures-fmt`, then read it and check: the output is one assistant message with `finish_reason` `stop` and one `uri` part, `modality: "audio"`, `mime_type: "audio/wav"` (sniffed), a `media://` uri, `transcript` set, and `id` / `expires_at` kept on the part; `output_preview` is the transcript; `media_refs` has one hash.
6. **Specs:**
   - Spec 01, `input_preview` row: append "A message with no text part previews its media transcripts."
   - Spec 02 § Media extraction step 1: add to the provider shapes "OpenAI chat output `message.audio` with `data`, which the OpenAI converter turns into an audio `blob` part carrying `transcript` (the format is not in the response, so the type is sniffed)".
   - Spec 02 § Truncation step 2: "text or reasoning `content`, a media part's `transcript`, or tool `arguments` / `response` when that value is a JSON string".
   - Spec 06 § Defaults (D164) text extraction: "…the concatenated text parts of its last assistant message (its media transcripts when it has no text part), or empty if there is none".
   - Spec 06 § Rendering (D163): "non-text parts as `[image]`, `[audio]` or `[file]`, or `[audio: <transcript>]` when the part has a transcript".

**Tests:**
- `payload.rs`: `a_voice_only_message_previews_its_transcripts` (two audio parts with transcripts → joined with `\n`); `text_wins_over_transcripts` (text plus audio with a transcript → only the text); `an_empty_transcript_gives_no_preview`.
- `truncate.rs`: `a_long_transcript_is_cut_like_text`, modelled on `a_huge_message_is_cut_at_its_leaf`: one **user** message (the first user message is never removed, so step 2 runs) whose only part is an audio `uri` with a transcript of a few thousand bytes (above the 1 KiB `LEAF_FLOOR`), a limit around 1500; `fit_messages` returns `Fitted::Messages`, the transcript is cut with the marker, and the serialized list fits.
- `openai.rs`: `output_audio_becomes_an_audio_blob` (fields as step 3, `id` and `expires_at` in the part's extra, no `audio` left in the message extra); `output_audio_without_data_stays_in_extra`; `a_non_string_audio_transcript_stays_in_the_part_extra`.
- `text.rs`: `media_renders_its_transcript` (`[audio: hello]`); `an_empty_transcript_renders_the_plain_placeholder`; `a_voice_only_reply_extracts_its_transcript` (last assistant message with only an audio part); `text_wins_over_transcripts_in_extraction`.
- The new golden case.

**Done when:**
- `cargo test -p cetra-otel -p cetra-eval` passes, including `--test golden`.
- `just fixtures-check` passes.
- `cargo clippy -p cetra-otel -p cetra-eval --all-targets -- -D warnings` and `cargo fmt --check` are clean.

**Commit:** `Use media transcripts in previews, truncation, OpenAI audio and eval`

---

## AP3 — The audio player

**Goal:** the Chat tab plays audio in a themed player with a waveform, and shows a media part's transcript under it.

**Edit:**
- `web/src/components/payload/audio-player.tsx` (new)
- `web/src/components/payload/audio-player.test.tsx` (new)
- `web/src/components/payload/waveform.ts` (new: pure functions and the peaks cache)
- `web/src/components/payload/waveform.test.ts` (new)
- `web/src/components/payload/media-view.tsx`
- `web/src/components/payload/media-view.test.tsx`

**Contracts:**

```ts
// waveform.ts
/** Bars drawn by the player. */
export const WAVEFORM_BARS = 64;
/** Files larger than this are not decoded; the player shows a plain progress bar. */
export const WAVEFORM_MAX_BYTES = 4 * 1024 * 1024;

/**
 * Peak amplitude of each of `bars` equal slices of `samples`, scaled so the largest is 1.
 * Silence (all zeros) gives all zeros. `bars` slices are made even when `samples` is shorter
 * than `bars` (empty slices are 0). Never NaN.
 */
export function peaksOf(samples: Float32Array, bars: number): number[];

/** "m:ss" for seconds ≥ 0; "h:mm:ss" from one hour; "0:00" for NaN or Infinity. */
export function clockOf(seconds: number): string;

/** Length label: "4.2 s" below one minute (one decimal), else clockOf ("1:05"). */
export function lengthOf(seconds: number): string;

/**
 * Returns null at once, before any fetch, when `globalThis.OfflineAudioContext` is undefined.
 * Otherwise fetches `src`, refuses bodies over WAVEFORM_MAX_BYTES (by Content-Length when
 * present, else by the bytes read), decodes with a one-channel, 8 kHz OfflineAudioContext and
 * returns peaksOf(first channel). Returns null when the fetch fails (network, CSP) or the
 * response is not ok, the body is too large or decoding fails. A fetch that throws or is not ok
 * is not cached (its entry is removed, so a later call retries); the other results, including
 * null, are cached per src in a module-level Map, and concurrent calls for one src share one
 * promise. An abort is not cached.
 */
export function loadPeaks(src: string, signal?: AbortSignal): Promise<number[] | null>;

/** Empties the peaks cache. For tests. */
export function resetPeaksCache(): void;
```

```tsx
// audio-player.tsx
export function AudioPlayer(props: {
  src: string;
  mimeType?: string;
  /** Decode for the waveform. False for external URLs not on this origin. */
  waveform: boolean;
}): JSX.Element;

/** The media, then the transcript under it, in one framed block. */
export function MediaWithTranscript(props: { transcript?: string; children: ReactNode }): JSX.Element;
```

**Steps:**
1. `waveform.ts` as the contracts above. `loadPeaks` aborts the read (via the signal or by cancelling the reader) once the byte cap is passed, and decodes in an `OfflineAudioContext`, which has nothing to close.
2. `AudioPlayer`:
   - A hidden `<audio preload="metadata" src={src}>` (keep the existing `biome-ignore lint/a11y/useMediaCaption` comment), driven by a ref. State: `playing`, `current` (s), `duration` (s or NaN), `failed`.
   - Events: `loadedmetadata` / `durationchange` set `duration`; `timeupdate` sets `current`; `play` / `pause` / `ended` set `playing` (`ended` also resets `current` to 0); `error` sets `failed`.
   - Layout, one row, `rounded-md border bg-muted/40 px-2 py-1.5`, at most `max-w-md` wide:
     - a `Button` `variant="ghost" size="icon-xs"`, `aria-label` "Play" or "Pause", icon `Play` / `Pause` from `lucide-react`;
     - the track (flex-1, h-8): when `peaks` is set, `WAVEFORM_BARS` vertical bars (min height 2 px, rounded), bars before `current / duration` in `bg-foreground`, the rest in `bg-muted-foreground/40`; otherwise a 4 px rounded bar with the played share in `bg-foreground` on `bg-muted-foreground/30`;
     - the clock `0:04 / 0:12` in `text-caption num text-muted-foreground` (`clockOf(current) / clockOf(duration)`, total `–:––` while unknown).
   - Under the row, a caption line `text-caption text-muted-foreground`: `audio/wav · 4.2 s` (the MIME type when given, then `lengthOf(duration)` once known, joined by ` · `).
   - The track is a slider: `role="slider"`, `aria-label` "Seek", `aria-valuemin` 0, `aria-valuemax` the duration rounded down (0 while unknown), `aria-valuenow` the current second rounded down, `aria-valuetext` `clockOf(current)`, `tabIndex` 0. Click sets `audio.currentTime` to the clicked share of the duration. ArrowLeft / ArrowRight seek −5 / +5 s (clamped), Home / End to 0 / duration. Space and Enter on the slider toggle play.
   - Waveform loading: when `waveform` is true, an `IntersectionObserver` on the track calls `loadPeaks(src, signal)` once the player is visible; the effect aborts on unmount and on `src` change. When `IntersectionObserver` is undefined, load nothing. A `null` result keeps the plain bar. (The test setup's `IntersectionObserver` stub reports every target visible at once, and jsdom has no `AudioContext`, so `loadPeaks` returns null without fetching in ordinary tests.)
   - The track is a `div` with `role="slider"`; add `// biome-ignore lint/a11y/useSemanticElements: the waveform is drawn inside the slider, which a range input cannot hold` (and the same pattern for any other a11y rule Biome raises on it, each with a reason).
   - `failed`: replace the row with `text-caption text-muted-foreground` "This audio cannot be played here" followed by the MIME type when known; the transcript (from `MediaWithTranscript`) still shows.
   - Play: `audio.play()` returns a promise; catch its rejection and set `failed` only when the error's name is `NotSupportedError`.
3. `MediaWithTranscript`: when `transcript` is a non-empty string, render
   `<figure className="flex max-w-md flex-col gap-1">{children}<figcaption className="text-body whitespace-pre-wrap break-words">{transcript}</figcaption></figure>`; otherwise render `children` alone.
4. `media-view.tsx`:
   - Read `transcript` like `mime` (`typeof part.transcript === "string" ? … : undefined`).
   - Replace `AudioControl` with `<AudioPlayer src mimeType={mime} waveform />` for blob, stored and inline sources, and `waveform={false}` for external sources after Load. Delete `AudioControl`.
   - Known limit, not to work around: the served UI's CSP (`crates/ui/src/lib.rs`, `media-src 'self'`, no `connect-src`) blocks `data:` audio and `fetch(data:…)`. Inline and small blob audio (under `media.min_bytes`, so rarely playable audio) therefore shows "This audio cannot be played here" with its transcript when served by `cetra serve`, and plays in the dev server. Stored `media://` audio, the normal case, is same-origin and works.
   - Wrap every branch's result (blob, dropped, external, stored/inline, chip) in `<MediaWithTranscript transcript={transcript}>`, so a dropped or unplayable audio still shows what was said, and an image with a transcript shows it too.
5. Dark and light: use only theme tokens (`bg-muted`, `text-muted-foreground`, `bg-foreground`, `border`); no raw colours.

**Tests:**
- `waveform.test.ts` (`afterEach(resetPeaksCache)`; each `loadPeaks` test uses its own `src` URL under `/api/v1/test-audio/…` with an MSW handler from `server.use`):
  - `peaksOf` scales to the loudest slice; silence gives zeros; fewer samples than bars gives `bars` values; negative samples count by magnitude.
  - `clockOf`: 0 → `0:00`, 4.2 → `0:04`, 65 → `1:05`, 3725 → `1:02:05`, NaN → `0:00`. `lengthOf`: 4.2 → `4.2 s`, 65 → `1:05`.
  - `loadPeaks returns null without OfflineAudioContext and fetches nothing` (a handler that records calls sees none).
  - With `vi.stubGlobal("OfflineAudioContext", FakeOfflineAudioContext)` (a class whose `decodeAudioData` resolves `{ getChannelData: () => new Float32Array([0, 0.5, -1, 0.25]) }` and whose no `close`; `vi.unstubAllGlobals()` after each): `decodes the first channel into peaks`; `refuses a body whose Content-Length is over the cap` (handler sets `Content-Length` to `WAVEFORM_MAX_BYTES + 1`; result null, `decodeAudioData` not called; if MSW rejects a header that does not match the body, stub `fetch` with `vi.stubGlobal` for this test and return `new Response(new Uint8Array(4), { headers: { "Content-Length": String(WAVEFORM_MAX_BYTES + 1) } })`); `fetches one src once` (two calls, one request); `returns null when decoding fails`.
- `audio-player.test.tsx` (stub `HTMLMediaElement.prototype.play` to resolve and dispatch `play`, and `pause` to dispatch `pause`; set `duration` with `Object.defineProperty` and dispatch `loadedmetadata`):
  - `shows the type and length once the metadata loads` (`audio/wav · 4.2 s`, clock `0:00 / 0:04`).
  - `toggles play and pause` (button label Play → Pause → Play).
  - `seeks with the arrow keys` (ArrowRight sets `currentTime` to 5, clamped at the duration; Home to 0). If jsdom's `currentTime` cannot be set, define it with `Object.defineProperty(audio, "currentTime", { value: 0, writable: true })` like `duration`.
  - `shows a message when the audio cannot be played` (dispatch `error`).
  - `does not fetch the file for the waveform without OfflineAudioContext` (the default jsdom environment; no request reaches MSW).
- `media-view.test.tsx`:
  - `renders a blob audio in the player with its transcript` (Play button present, transcript text visible, no `controls` audio element).
  - `keeps the transcript of a dropped audio` (dropped label and transcript).
  - `an external audio loads the player without a waveform after Load`.
  - Existing tests keep passing.

**Done when** (from the worktree root, with `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$HOME/.npm-global/bin:$PATH`; run `just js-install` first if `node_modules/` is missing):
- `node_modules/.bin/biome check web/src/components/payload` is clean.
- `pnpm --dir web gen:routes && cd web && ./node_modules/.bin/vitest run src/components/payload --maxWorkers=8` passes.
- `pnpm --dir web run typecheck` passes.

**Commit:** `Play audio in a themed player with its transcript`

---

## AP4 — Docs, decision and changelog

**Goal:** the decision, the UI spec, the user docs and the changelog describe the change.

**Edit:**
- `docs/mvp-decisions.md` (Track C section: add D-C-29 after the last D-C row)
- `docs/spec/11-ui.md` (the *Chat* bullet)
- `website/src/content/docs/docs/tracing/media.mdx`
- `CHANGELOG.md`

`docs/roadmap.md` is not edited: no track changes status. `skills/references.json` does not list the media page, so no skill reference is regenerated.

**Steps:**
1. D-C-29, as a new row right after the row at the highest line number of the Track C table (the D-C rows are not in numeric order) (D-C-28 is on the unmerged `claude/session-page` branch; whichever merges second rebases and keeps both rows in order), in the table format of the Track C rows: the summary from this plan's decisions table, and "Alternatives rejected: pairing an audio part with the next text part (no data-model change, but ambiguous when a text instruction follows audio); a progress bar only (no waveform)."
2. Spec 11 *Chat* bullet: replace "images/audio/PDF rendered from `media://` refs" with "images and PDFs rendered from `media://` refs; audio in a themed player (play/pause, a waveform decoded once visible for files up to 4 MiB, else a progress bar, elapsed and total time, MIME type and length), and a media part's `transcript` under the media in the same block (D-C-29)".
3. `media.mdx`: add a section `## Transcripts` after `## Send media`: a media part may carry `transcript`, the text of what was said; cetra keeps it when it stores or drops the media; for OpenAI chat completions with audio output, the transcript the API returns is kept; a message with only audio shows its transcripts as its preview; the trace view plays audio with a waveform and shows the transcript under the player; judges read `[audio: <transcript>]`. Include one `json` example of an audio part with a transcript (`check-samples.mjs` checks only Python and TypeScript fences, so a `json` fence needs no marker). Also update the "In the UI" paragraph (~line 97) to say audio plays in a player with a waveform and its transcript under it. Follow `website/STYLE.md`.
4. `CHANGELOG.md` § `[Unreleased]`:
   - `### Added`: "Media parts carry an optional `transcript`. OpenAI chat output audio keeps its transcript, a message with only audio previews its transcripts, and judges read `[audio: <transcript>]`."
   - `### Changed`: "The trace view plays audio in a themed player with a waveform and shows its transcript under it."

**Tests:** the docs checks.

**Done when:**
- `just check-docs` passes (with the Node PATH export above).
- `git diff --stat` touches only the Edit list.

**Commit:** `Document media transcripts and the audio player`

---

After AP4: `just check` once, a code review, and a browser check of the player on a real server in both themes at 900 px.
