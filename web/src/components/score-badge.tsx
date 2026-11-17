import { useQuery } from "@tanstack/react-query";
import { cn } from "cn";
import { type ComponentProps, createContext, type ReactNode, useContext, useMemo } from "react";
import { liveScoreDefinitionsQuery } from "@/api/queries-evaluation";
import type { Schema } from "@/api/types";
import { useProject } from "@/components/shell/project-context";
import { averageGoodness, goodness, type ScoreTone, toneOf } from "@/lib/score-tone";

const TONE_CLASSES: Record<ScoreTone, string> = {
  good: "tone-good",
  mid: "tone-mid",
  bad: "tone-bad",
};

type Definitions = ReadonlyMap<string, Schema<"ScoreDefinition">>;

const ScoreDefinitionsContext = createContext<Definitions | null>(null);

/**
 * Loads the live score definitions of the current project once and provides them by name to
 * every score badge below it. While loading, or when loading failed, the map is empty.
 */
export function ScoreDefinitionsProvider({ children }: { children: ReactNode }) {
  const { org, project } = useProject();
  const definitions = useQuery(liveScoreDefinitionsQuery(org, project));
  const byName = useMemo<Definitions>(() => {
    const map = new Map<string, Schema<"ScoreDefinition">>();
    if (definitions.isSuccess) {
      for (const definition of definitions.data.data) {
        if (!map.has(definition.name)) {
          map.set(definition.name, definition);
        }
      }
    }
    return map;
  }, [definitions.isSuccess, definitions.data]);
  return (
    <ScoreDefinitionsContext.Provider value={byName}>{children}</ScoreDefinitionsContext.Provider>
  );
}

/** The live definition of `name` in the current project, if it has loaded. */
function useDefinition(name: string): Schema<"ScoreDefinition"> | undefined {
  const definitions = useContext(ScoreDefinitionsContext);
  if (!definitions) {
    throw new Error("Score badges must be used within a ScoreDefinitionsProvider");
  }
  return definitions.get(name);
}
/** The text in a pill of its tone, or plain when the score is neutral or not loaded yet. */
function Toned({
  g,
  text,
  className,
  ...rest
}: { g: number | undefined; text: string } & ComponentProps<"span">) {
  const tone = g === undefined ? undefined : toneOf(g);
  return (
    <span {...rest} className={cn(className, tone && ["rounded-sm px-1", TONE_CLASSES[tone]])}>
      {text}
    </span>
  );
}

/** One score value, coloured by its definition's direction. `text` is what the caller shows today. */
export function ScoreBadge({
  name,
  value,
  text,
  ...rest
}: {
  name: string;
  value: Schema<"ScoreValue">;
  text: string;
} & ComponentProps<"span">) {
  const definition = useDefinition(name);
  const g = definition ? goodness(value, definition) : undefined;
  return <Toned g={g} text={text} {...rest} />;
}

/** A user's average score, coloured by its definition's direction. `text` is what the caller shows today. */
export function ScoreAverageBadge({
  name,
  avg,
  dataType,
  text,
  ...rest
}: {
  name: string;
  avg: number;
  dataType: Schema<"ScoreDataType">;
  text: string;
} & ComponentProps<"span">) {
  const definition = useDefinition(name);
  const g = definition ? averageGoodness(avg, dataType, definition) : undefined;
  return <Toned g={g} text={text} {...rest} />;
}
