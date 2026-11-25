import { cn } from "cn";
import { Pause, Play } from "lucide-react";
import {
  type KeyboardEvent,
  type MouseEvent,
  type ReactNode,
  useEffect,
  useRef,
  useState,
} from "react";
import { clockOf, lengthOf, loadPeaks, WAVEFORM_BARS } from "@/components/payload/waveform";
import { Button } from "@/components/ui/button";

/** Height of the waveform track in pixels (h-8). */
const TRACK_PX = 32;
/** Seconds moved by one ArrowLeft or ArrowRight press. */
const SEEK_STEP = 5;

export function AudioPlayer({
  src,
  mimeType,
  waveform,
}: {
  src: string;
  mimeType?: string;
  /** Decode for the waveform. False for external URLs not on this origin. */
  waveform: boolean;
}) {
  const audioRef = useRef<HTMLAudioElement>(null);
  const trackRef = useRef<HTMLDivElement>(null);
  const [playing, setPlaying] = useState(false);
  const [current, setCurrent] = useState(0);
  const [duration, setDuration] = useState(Number.NaN);
  const [failed, setFailed] = useState(false);
  const [peaks, setPeaks] = useState<number[] | null>(null);

  useEffect(() => {
    setPeaks(null);
    if (!waveform) return;
    const track = trackRef.current;
    if (!track || typeof IntersectionObserver === "undefined") return;
    const controller = new AbortController();
    let requested = false;
    const observer = new IntersectionObserver((entries) => {
      if (requested || !entries.some((entry) => entry.isIntersecting)) return;
      requested = true;
      observer.disconnect();
      loadPeaks(src, controller.signal).then((result) => {
        if (!controller.signal.aborted && result) setPeaks(result);
      });
    });
    observer.observe(track);
    return () => {
      controller.abort();
      observer.disconnect();
    };
  }, [src, waveform]);

  const hasDuration = Number.isFinite(duration) && duration > 0;
  const progress = hasDuration ? Math.min(current / duration, 1) : 0;
  const caption = [mimeType, Number.isFinite(duration) ? lengthOf(duration) : undefined]
    .filter((part): part is string => typeof part === "string" && part.length > 0)
    .join(" · ");

  function play() {
    const audio = audioRef.current;
    if (!audio) return;
    audio.play().catch((error: unknown) => {
      if (isNotSupported(error)) setFailed(true);
    });
  }

  function toggle() {
    const audio = audioRef.current;
    if (!audio) return;
    if (playing) audio.pause();
    else play();
  }

  function seek(seconds: number) {
    const audio = audioRef.current;
    if (!audio) return;
    const upper = hasDuration ? duration : 0;
    const target = Math.min(Math.max(seconds, 0), upper);
    audio.currentTime = target;
    setCurrent(target);
  }

  function onTrackClick(event: MouseEvent<HTMLDivElement>) {
    const rect = event.currentTarget.getBoundingClientRect();
    if (!hasDuration || rect.width <= 0) return;
    const share = Math.min(Math.max((event.clientX - rect.left) / rect.width, 0), 1);
    seek(share * duration);
  }

  function onTrackKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    switch (event.key) {
      case " ":
      case "Enter":
        event.preventDefault();
        toggle();
        return;
      case "Home":
        event.preventDefault();
        seek(0);
        return;
      case "End":
        if (!hasDuration) return;
        event.preventDefault();
        seek(duration);
        return;
      case "ArrowLeft":
      case "ArrowRight":
        if (!hasDuration) return;
        event.preventDefault();
        seek(current + (event.key === "ArrowRight" ? SEEK_STEP : -SEEK_STEP));
        return;
      default:
        return;
    }
  }

  return (
    <div className="flex max-w-md flex-col gap-1">
      {/* biome-ignore lint/a11y/useMediaCaption: ingested audio has no captions */}
      <audio
        ref={audioRef}
        preload="metadata"
        src={src}
        className="hidden"
        onLoadedMetadata={(event) => setDuration(event.currentTarget.duration)}
        onDurationChange={(event) => setDuration(event.currentTarget.duration)}
        onTimeUpdate={(event) => setCurrent(event.currentTarget.currentTime)}
        onPlay={() => setPlaying(true)}
        onPause={() => setPlaying(false)}
        onEnded={(event) => {
          setPlaying(false);
          event.currentTarget.currentTime = 0;
          setCurrent(0);
        }}
        onError={() => setFailed(true)}
      />
      {failed ? (
        <p className="text-caption text-muted-foreground">
          {mimeType
            ? `This audio cannot be played here · ${mimeType}`
            : "This audio cannot be played here"}
        </p>
      ) : (
        <>
          <div className="flex items-center gap-2 rounded-md border bg-muted/40 px-2 py-1.5">
            <Button
              type="button"
              variant="ghost"
              size="icon-xs"
              aria-label={playing ? "Pause" : "Play"}
              onClick={toggle}
            >
              {playing ? <Pause /> : <Play />}
            </Button>
            <div
              ref={trackRef}
              role="slider"
              aria-label="Seek"
              aria-valuemin={0}
              aria-valuemax={hasDuration ? Math.floor(duration) : 0}
              aria-valuenow={Math.floor(current)}
              aria-valuetext={clockOf(current)}
              tabIndex={0}
              onClick={onTrackClick}
              onKeyDown={onTrackKeyDown}
              className="flex h-8 min-w-0 flex-1 cursor-pointer items-center rounded-sm outline-none focus-visible:outline-2 focus-visible:outline-ring focus-visible:outline-offset-2 focus-visible:outline-solid"
            >
              {peaks ? (
                <span className="flex h-full w-full items-center gap-px" aria-hidden>
                  {peaks.slice(0, WAVEFORM_BARS).map((peak, index) => (
                    <span
                      // biome-ignore lint/suspicious/noArrayIndexKey: the bars are a fixed sequence of the same length
                      key={index}
                      className={cn(
                        "min-w-0 flex-1 rounded-full",
                        index / WAVEFORM_BARS < progress
                          ? "bg-foreground"
                          : "bg-muted-foreground/40",
                      )}
                      style={{ height: `${Math.max(2, peak * TRACK_PX)}px` }}
                    />
                  ))}
                </span>
              ) : (
                <span className="relative block h-1 w-full overflow-hidden rounded-full bg-muted-foreground/30">
                  <span
                    className="absolute inset-y-0 left-0 bg-foreground"
                    style={{ width: `${progress * 100}%` }}
                  />
                </span>
              )}
            </div>
            <span className="text-caption text-muted-foreground tabular-nums whitespace-nowrap">
              {`${clockOf(current)} / ${hasDuration ? clockOf(duration) : "–:––"}`}
            </span>
          </div>
          {caption ? <p className="text-caption text-muted-foreground">{caption}</p> : null}
        </>
      )}
    </div>
  );
}

/** The media, then the transcript under it, in one framed block. */
export function MediaWithTranscript({
  transcript,
  children,
}: {
  transcript?: string;
  children: ReactNode;
}) {
  if (typeof transcript !== "string" || transcript.length === 0) return <>{children}</>;
  return (
    <figure className="flex max-w-md flex-col gap-1">
      {children}
      <figcaption className="text-body break-words whitespace-pre-wrap">{transcript}</figcaption>
    </figure>
  );
}

function isNotSupported(error: unknown): boolean {
  return (
    typeof error === "object" &&
    error !== null &&
    "name" in error &&
    error.name === "NotSupportedError"
  );
}
