import { type HttpHandler, HttpResponse, http } from "msw";
import projectFixture from "../../../fixtures/api/static/project.json";
import projectsFixture from "../../../fixtures/api/static/projects.json";
import activityFixture from "../../../fixtures/api/static/projects-activity.json";

/**
 * The story projects of the large mock's org (M16, design § 13): one project per story, each with
 * its own generated traces. The mock answers the project list, one project and the activity for
 * the three new projects; the support project and the playground keep the fixtures.
 */

export type StoryId = "support" | "coding" | "voice" | "extraction";

export type StoryProject = {
  story: StoryId;
  /** URL slug, as in `/acme/<slug>/traces`. */
  slug: string;
  /** The API's project ref, `acme/<slug>` (MSW hands `params.project` decoded). */
  ref: string;
  id: string;
  name: string;
};

export const STORY_PROJECTS: readonly [StoryProject, StoryProject, StoryProject, StoryProject] = [
  {
    story: "support",
    slug: "support-bot",
    ref: "acme/support-bot",
    id: "01900000-0000-7000-8000-000000000001",
    name: "Support bot",
  },
  {
    story: "coding",
    slug: "code-agent",
    ref: "acme/code-agent",
    id: "01900000-0000-7000-8000-000000000003",
    name: "Code agent",
  },
  {
    story: "voice",
    slug: "voice-desk",
    ref: "acme/voice-desk",
    id: "01900000-0000-7000-8000-000000000004",
    name: "Voice desk",
  },
  {
    story: "extraction",
    slug: "invoice-pipeline",
    ref: "acme/invoice-pipeline",
    id: "01900000-0000-7000-8000-000000000005",
    name: "Invoice pipeline",
  },
];

/** The org of the large mock. */
const ORG = "acme";

/** The story's project. */
export function storyProject(story: StoryId): StoryProject {
  const project = STORY_PROJECTS.find((candidate) => candidate.story === story);
  if (!project) {
    // Unreachable: `STORY_PROJECTS` has one entry per `StoryId`.
    throw new Error(`no project for story ${story}`);
  }
  return project;
}

/** The project of an API ref (`acme/<slug>`), when it is a story project. */
export function projectOfRef(ref: string | undefined): StoryProject | undefined {
  return STORY_PROJECTS.find((project) => project.ref === ref);
}

/** The three story projects beside the fixtures' projects, in list order. */
const NEW_PROJECTS = STORY_PROJECTS.filter((project) => project.story !== "support");

/** The new projects' activity, with their totals; the buckets are scaled from the support entry. */
const NEW_ACTIVITY: { story: StoryId; traces: number; errors: number; cost_usd: number }[] = [
  { story: "coding", traces: 18, errors: 2, cost_usd: 6.42 },
  { story: "voice", traces: 64, errors: 2, cost_usd: 0.19 },
  { story: "extraction", traces: 120, errors: 6, cost_usd: 0.31 },
];

type Activity = (typeof activityFixture.data)[number];

/**
 * `total` split over the buckets in the proportions of `weights`: each bucket gets the floor of its
 * share, and the last bucket the remainder, so the buckets sum to `total`.
 */
function scaled(total: number, weights: number[]): number[] {
  const sum = weights.reduce((acc, weight) => acc + weight, 0);
  const floors = weights.map((weight) => Math.floor((total * weight) / sum));
  const remainder = total - floors.reduce((acc, count) => acc + count, 0);
  return floors.map((count, i) => (i === floors.length - 1 ? count + remainder : count));
}

/** The activity entry of a new project: the support entry's buckets, scaled to its totals. */
function activityOf(base: Activity, story: (typeof NEW_ACTIVITY)[number], id: string): Activity {
  const traces = scaled(
    story.traces,
    base.buckets.map((bucket) => bucket.traces),
  );
  const errors = scaled(
    story.errors,
    base.buckets.map((bucket) => bucket.errors),
  );
  return {
    ...base,
    project_id: id,
    traces: story.traces,
    errors: story.errors,
    cost_usd: story.cost_usd,
    buckets: traces.map((count, i) => ({ traces: count, errors: errors[i] ?? 0 })),
  };
}

const projectsHandler = http.get("/api/v1/orgs/:org/projects", ({ params }) => {
  if (params.org !== ORG) {
    return undefined;
  }
  return HttpResponse.json({
    data: [
      ...projectsFixture.data,
      ...NEW_PROJECTS.map((project) => ({
        ...projectsFixture.data[0],
        id: project.id,
        slug: project.slug,
        name: project.name,
      })),
    ],
    next_cursor: null,
  });
});

const projectHandler = http.get("/api/v1/projects/:project", ({ params }) => {
  const project = projectOfRef(String(params.project));
  if (!project || project.story === "support") {
    return undefined;
  }
  return HttpResponse.json({
    ...projectFixture,
    id: project.id,
    slug: project.slug,
    name: project.name,
  });
});

const activityHandler = http.get("/api/v1/orgs/:org/projects/activity", ({ params }) => {
  const [base] = activityFixture.data;
  if (params.org !== ORG || !base) {
    return undefined;
  }
  return HttpResponse.json({
    ...activityFixture,
    data: [
      ...activityFixture.data,
      ...NEW_ACTIVITY.map((story) => activityOf(base, story, storyProject(story.story).id)),
    ],
  });
});

/**
 * The story projects' handlers. Each answers only what it adds and returns nothing otherwise, so
 * the fixtures answer the rest. Not registered here: the large mock installs them.
 */
export const projectHandlers: HttpHandler[] = [projectsHandler, projectHandler, activityHandler];
