import { fireEvent, render, screen } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { AudioPlayer } from "@/components/payload/audio-player";
import { resetPeaksCache } from "@/components/payload/waveform";
import { server } from "@/mocks/node";

const src = "/api/v1/test-audio/player.wav";

function audioElement(): HTMLAudioElement {
  const audio = document.querySelector("audio");
  if (!audio) throw new Error("no audio element");
  return audio;
}

function setDuration(audio: HTMLAudioElement, seconds: number) {
  Object.defineProperty(audio, "duration", { value: seconds, configurable: true });
  fireEvent(audio, new Event("loadedmetadata"));
}

beforeEach(() => {
  vi.spyOn(HTMLMediaElement.prototype, "play").mockImplementation(function play(
    this: HTMLMediaElement,
  ) {
    fireEvent(this, new Event("play"));
    return Promise.resolve();
  });
  vi.spyOn(HTMLMediaElement.prototype, "pause").mockImplementation(function pause(
    this: HTMLMediaElement,
  ) {
    fireEvent(this, new Event("pause"));
  });
});

afterEach(() => {
  vi.restoreAllMocks();
  resetPeaksCache();
});

it("shows the type and length once the metadata loads", () => {
  render(<AudioPlayer src={src} mimeType="audio/wav" waveform={false} />);
  setDuration(audioElement(), 4.2);
  expect(screen.getByText("audio/wav · 4.2 s")).toBeInTheDocument();
  expect(screen.getByText("0:00 / 0:04")).toBeInTheDocument();
});

it("toggles play and pause", () => {
  render(<AudioPlayer src={src} mimeType="audio/wav" waveform={false} />);
  fireEvent.click(screen.getByRole("button", { name: "Play" }));
  expect(screen.getByRole("button", { name: "Pause" })).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Pause" }));
  expect(screen.getByRole("button", { name: "Play" })).toBeInTheDocument();
});

it("seeks with the arrow keys and Home", () => {
  render(<AudioPlayer src={src} mimeType="audio/wav" waveform={false} />);
  const audio = audioElement();
  Object.defineProperty(audio, "currentTime", { value: 0, writable: true, configurable: true });
  setDuration(audio, 8);
  const slider = screen.getByRole("slider", { name: "Seek" });
  fireEvent.keyDown(slider, { key: "ArrowRight" });
  expect(audio.currentTime).toBe(5);
  fireEvent.keyDown(slider, { key: "ArrowRight" });
  expect(audio.currentTime).toBe(8);
  fireEvent.keyDown(slider, { key: "Home" });
  expect(audio.currentTime).toBe(0);
});

it("shows a message when the audio cannot be played", () => {
  render(<AudioPlayer src={src} mimeType="audio/wav" waveform={false} />);
  fireEvent(audioElement(), new Event("error"));
  expect(screen.getByText("This audio cannot be played here · audio/wav")).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Play" })).not.toBeInTheDocument();
});

it("does not fetch the file for the waveform without AudioContext", async () => {
  const requests = vi.fn();
  server.use(
    http.get(src, () => {
      requests();
      return HttpResponse.arrayBuffer(new ArrayBuffer(4));
    }),
  );
  render(<AudioPlayer src={src} mimeType="audio/wav" waveform />);
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(requests).not.toHaveBeenCalled();
});
