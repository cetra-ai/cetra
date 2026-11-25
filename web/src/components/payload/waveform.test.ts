import { HttpResponse, http } from "msw";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  clockOf,
  lengthOf,
  loadPeaks,
  peaksOf,
  resetPeaksCache,
  WAVEFORM_BARS,
  WAVEFORM_MAX_BYTES,
} from "@/components/payload/waveform";
import { server } from "@/mocks/node";

afterEach(() => {
  resetPeaksCache();
  vi.unstubAllGlobals();
});

describe("peaksOf", () => {
  it("scales to the loudest slice", () => {
    expect(peaksOf(new Float32Array([0, 0.5, -1, 0.25]), 2)).toEqual([0.5, 1]);
  });

  it("gives zeros for silence", () => {
    expect(peaksOf(new Float32Array(8), 4)).toEqual([0, 0, 0, 0]);
  });

  it("returns one value per bar when there are fewer samples than bars", () => {
    const peaks = peaksOf(new Float32Array([1, -1]), WAVEFORM_BARS);
    expect(peaks).toHaveLength(WAVEFORM_BARS);
    expect(peaks.every((peak) => Number.isFinite(peak))).toBe(true);
  });

  it("counts negative samples by magnitude", () => {
    expect(peaksOf(new Float32Array([-1, 0.5]), 2)).toEqual([1, 0.5]);
  });

  it("never returns NaN for non-finite samples", () => {
    const peaks = peaksOf(new Float32Array([Number.NaN, 0.5, Number.POSITIVE_INFINITY, 0.25]), 2);
    expect(peaks.every((peak) => Number.isFinite(peak))).toBe(true);
  });
});

describe("clockOf and lengthOf", () => {
  it.each([
    [0, "0:00"],
    [4.2, "0:04"],
    [65, "1:05"],
    [3725, "1:02:05"],
    [Number.NaN, "0:00"],
    [Number.POSITIVE_INFINITY, "0:00"],
  ])("clockOf(%s) is %s", (seconds, expected) => {
    expect(clockOf(seconds)).toBe(expected);
  });

  it.each([
    [4.2, "4.2 s"],
    [65, "1:05"],
  ])("lengthOf(%s) is %s", (seconds, expected) => {
    expect(lengthOf(seconds)).toBe(expected);
  });
});

describe("loadPeaks", () => {
  const audioUrl = (name: string) => `/api/v1/test-audio/${name}`;

  it("returns null without AudioContext and fetches nothing", async () => {
    const requests = vi.fn();
    server.use(
      http.get(audioUrl("no-context.wav"), () => {
        requests();
        return HttpResponse.arrayBuffer(new ArrayBuffer(4));
      }),
    );
    await expect(loadPeaks(audioUrl("no-context.wav"))).resolves.toBeNull();
    expect(requests).not.toHaveBeenCalled();
  });

  describe("with a fake AudioContext", () => {
    const decodeAudioData = vi.fn();

    class FakeAudioContext {
      decodeAudioData(data: ArrayBuffer) {
        decodeAudioData(data);
        return Promise.resolve({ getChannelData: () => new Float32Array([0, 0.5, -1, 0.25]) });
      }
      close() {
        return Promise.resolve();
      }
    }

    function stubContext() {
      decodeAudioData.mockClear();
      vi.stubGlobal("AudioContext", FakeAudioContext);
    }

    it("decodes the first channel into peaks", async () => {
      stubContext();
      server.use(
        http.get(audioUrl("decode.wav"), () => HttpResponse.arrayBuffer(new ArrayBuffer(4))),
      );
      const peaks = await loadPeaks(audioUrl("decode.wav"));
      expect(decodeAudioData).toHaveBeenCalledTimes(1);
      expect(peaks).toHaveLength(WAVEFORM_BARS);
      expect(Math.max(...(peaks ?? []))).toBe(1);
    });

    it("refuses a body whose Content-Length is over the cap", async () => {
      stubContext();
      const oversized = new Response(new Uint8Array(4), {
        headers: { "Content-Length": String(WAVEFORM_MAX_BYTES + 1) },
      });
      vi.stubGlobal(
        "fetch",
        vi.fn(() => Promise.resolve(oversized)),
      );
      await expect(loadPeaks("/api/v1/test-audio/too-large.wav")).resolves.toBeNull();
      expect(decodeAudioData).not.toHaveBeenCalled();
    });

    it("fetches one src once", async () => {
      stubContext();
      const requests = vi.fn();
      server.use(
        http.get(audioUrl("once.wav"), () => {
          requests();
          return HttpResponse.arrayBuffer(new ArrayBuffer(4));
        }),
      );
      const [first, second] = await Promise.all([
        loadPeaks(audioUrl("once.wav")),
        loadPeaks(audioUrl("once.wav")),
      ]);
      expect(requests).toHaveBeenCalledTimes(1);
      expect(first).toEqual(second);
    });

    it("returns null when decoding fails", async () => {
      vi.stubGlobal(
        "AudioContext",
        class {
          decodeAudioData() {
            return Promise.reject(new Error("unsupported"));
          }
          close() {
            return Promise.resolve();
          }
        },
      );
      server.use(
        http.get(audioUrl("broken.wav"), () => HttpResponse.arrayBuffer(new ArrayBuffer(4))),
      );
      await expect(loadPeaks(audioUrl("broken.wav"))).resolves.toBeNull();
    });
  });
});
