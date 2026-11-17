import { useQuery } from "@tanstack/react-query";
import { cn } from "cn";
import type { ComponentProps } from "react";
import { liveScoreDefinitionsQuery } from "@/api/queries-evaluation";
import type { Schema } from "@/api/types";
import { useProject } from "@/components/shell/project-context";
import { averageGoodness, goodness, type ScoreTone, toneOf } from "@/lib/score-tone";

const TONE_CLASSES: Record<ScoreTone, string> = {
  good: "bg-status-ok/15 text-status-ok",
  mid: "bg-status-warn/15 text-status-warn",
  bad: "bg-status-error/15 text-status-error",
};

/** The live definition of `name` in the current project, if it has loaded. */
function useDefinition(name: string): Schema<"ScoreDefinition"> | undefined {
  const { org, project } = useProject();
  const definitions = useQuery(liveScoreDefinitionsQuery(org, project));
  return definitions.data?.data.find((definition) => definition.name === name);
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
