/** Bars drawn by the player. */
export const WAVEFORM_BARS = 64;
/** Files larger than this are not decoded; the player shows a plain progress bar. */
export const WAVEFORM_MAX_BYTES = 4 * 1024 * 1024;

/**
 * Peak amplitude of each of `bars` equal slices of `samples`, scaled so the largest is 1.
 * Silence (all zeros) gives all zeros. `bars` slices are made even when `samples` is shorter
 * than `bars` (empty slices are 0). Never NaN.
 */
export function peaksOf(samples: Float32Array, bars: number): number[] {
  const peaks: number[] = [];
  let loudest = 0;
  for (let bar = 0; bar < bars; bar++) {
    const start = Math.floor((bar * samples.length) / bars);
    const end = Math.floor(((bar + 1) * samples.length) / bars);
    let peak = 0;
    for (let i = start; i < end; i++) {
      const magnitude = Math.abs(samples[i] ?? 0);
      if (Number.isFinite(magnitude) && magnitude > peak) peak = magnitude;
    }
    peaks.push(peak);
    if (peak > loudest) loudest = peak;
  }
  if (loudest === 0) return peaks;
  return peaks.map((peak) => peak / loudest);
}

/** "m:ss" for seconds ≥ 0; "h:mm:ss" from one hour; "0:00" for NaN or Infinity. */
export function clockOf(seconds: number): string {
  const safe = Number.isFinite(seconds) && seconds > 0 ? Math.floor(seconds) : 0;
  const hours = Math.floor(safe / 3600);
  const minutes = Math.floor((safe % 3600) / 60);
  const secs = String(safe % 60).padStart(2, "0");
  if (hours > 0) return `${hours}:${String(minutes).padStart(2, "0")}:${secs}`;
  return `${minutes}:${secs}`;
}

/** Length label: "4.2 s" below one minute (one decimal), else clockOf ("1:05"). */
export function lengthOf(seconds: number): string {
  if (Number.isFinite(seconds) && seconds >= 0) {
    const tenths = Math.round(seconds * 10);
    if (tenths < 600) return `${(tenths / 10).toFixed(1)} s`;
  }
  return clockOf(seconds);
}

/** Peaks per source, shared by every player of that source. Null means "no waveform". */
const peaksCache = new Map<string, Promise<number[] | null>>();

/**
 * Returns null at once, before any fetch, when `globalThis.OfflineAudioContext` is undefined.
 * Otherwise fetches `src`, refuses bodies over WAVEFORM_MAX_BYTES (by Content-Length when
 * present, else by the bytes read), decodes with a one-channel, 8 kHz OfflineAudioContext
 * (decodeAudioData resamples to that rate, which bounds the decoded size) and returns
 * peaksOf(first channel). Returns null when the fetch fails (network, CSP) or the response is
 * not ok, the body is too large or decoding fails. A decoded result, an over-the-cap body and
 * a decode failure are cached per src in a module-level Map; a failed fetch (a thrown fetch or
 * a response that is not ok) is removed from the Map once it settles, so a later call retries.
 * Concurrent calls for one src share one promise. An abort is not cached.
 *
 * The decode itself is not tied to the signal: an aborted caller stops waiting and gets null,
 * while the shared decode still runs to its end (bounded by WAVEFORM_MAX_BYTES) and is cached.
 * Tying it to one caller would hand null to every other caller sharing it, for example after
 * React's StrictMode mounts the effect twice.
 */
export function loadPeaks(src: string, signal?: AbortSignal): Promise<number[] | null> {
  if (typeof globalThis.OfflineAudioContext === "undefined") return Promise.resolve(null);
  if (signal?.aborted) return Promise.resolve(null);
  let shared = peaksCache.get(src);
  if (!shared) {
    shared = decodePeaks(src);
    peaksCache.set(src, shared);
  }
  if (!signal) return shared;
  const pending = shared;
  return new Promise((resolve) => {
    const onAbort = () => resolve(null);
    signal.addEventListener("abort", onAbort, { once: true });
    pending.then((peaks) => {
      signal.removeEventListener("abort", onAbort);
      resolve(peaks);
    });
  });
}

/** Empties the peaks cache. For tests. */
export function resetPeaksCache(): void {
  peaksCache.clear();
}

/**
 * The peaks of `src`, or null. A fetch that throws or answers not ok is a transient failure:
 * its cache entry is deleted once the shared promise settles, so a later call fetches again.
 */
async function decodePeaks(src: string): Promise<number[] | null> {
  let response: Response;
  try {
    response = await fetch(src);
  } catch {
    peaksCache.delete(src);
    return null;
  }
  if (!response.ok) {
    peaksCache.delete(src);
    return null;
  }
  try {
    const declared = response.headers.get("Content-Length");
    if (declared !== null && Number(declared) > WAVEFORM_MAX_BYTES) {
      await response.body?.cancel();
      return null;
    }
    const bytes = await readCapped(response);
    if (!bytes) return null;
    const context = new OfflineAudioContext(1, 1, 8000);
    const decoded = await context.decodeAudioData(bytes);
    return peaksOf(decoded.getChannelData(0), WAVEFORM_BARS);
  } catch {
    return null;
  }
}

/** The body's bytes, or null once they pass WAVEFORM_MAX_BYTES (the rest of the body is cancelled). */
async function readCapped(response: Response): Promise<ArrayBuffer | null> {
  if (!response.body) {
    const buffer = await response.arrayBuffer();
    return buffer.byteLength > WAVEFORM_MAX_BYTES ? null : buffer;
  }
  const reader = response.body.getReader();
  const chunks: Uint8Array[] = [];
  let total = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    total += value.byteLength;
    if (total > WAVEFORM_MAX_BYTES) {
      await reader.cancel();
      return null;
    }
    chunks.push(value);
  }
  const joined = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    joined.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return joined.buffer;
}
