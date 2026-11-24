// The homepage's four stories (design §12, D-W-24, D-W-25): for each of the seven pictures (the
// hero and the six product chapters), the screenshot's name, its Frame label, its alt and its
// one-line caption. The names are the shots of website/screenshots.json; StoryPicture.astro turns
// a name into an image, because only a bundler import gives Astro's image pipeline its metadata.
// stories.test.mjs checks the shapes, the names and the words.

/** In the order they are shown. The first is the default (no JavaScript, or no or an unknown `?story=`). */
export const STORIES = /** @type {const} */ ([
  { id: "support", label: "Support agent" },
  { id: "coding", label: "Coding agent" },
  { id: "voice", label: "Voice agent" },
  { id: "extraction", label: "Document extraction" },
]);

/**
 * @typedef {object} PictureContent
 * @property {string} alt     what the picture shows, one sentence, for screen readers
 * @property {string} caption one line under the picture
 */

/**
 * @typedef {object} Picture
 * @property {string} id      "hero" or the chapter's id without "ch-"
 * @property {string} section the page's name, for the picker's accessible name
 * @property {string} base    the shot's base name in screenshots.json
 * @property {string | Record<string, string>} label the Frame's title-bar text; one per story when a record
 * @property {Record<string, PictureContent>} stories keyed by story id
 */

/** @type {Picture[]} */
export const PICTURES = [
  {
    id: "hero",
    section: "Trace view",
    base: "marketing-trace-view",
    label: {
      support: "Traces · Support bot",
      coding: "Traces · Coding agent",
      voice: "Traces · Voice agent",
      extraction: "Traces · Document pipeline",
    },
    stories: {
      support: {
        alt: "The cetra trace view: a support agent's trace, with its span tree, timeline, models, tokens and costs, and the root span's input and output.",
        caption: "See what each step of a support answer cost and how long it took.",
      },
      coding: {
        alt: "The cetra trace view: a coding agent's trace, where the first test run fails and the second passes after a fix, with its timeline, models, tokens and costs.",
        caption: "Find the failed test run, and the fix that followed, in one trace.",
      },
      voice: {
        alt: "The cetra trace view: a voice agent's trace for one caller turn, where a 1.22 s availability check is the longest step, with its timeline, models and costs.",
        caption: "Find the one slow tool call behind a voice turn that took 3.1 s.",
      },
      extraction: {
        alt: "The cetra trace view: an invoice extraction pipeline, where the schema check runs on the first extraction and the fields are extracted again, with its timeline and costs.",
        caption:
          "See the schema check send an invoice back for a second extraction, and what it cost.",
      },
    },
  },
  {
    id: "trace",
    section: "Trace list",
    base: "marketing-trace-list",
    label: "Traces",
    stories: {
      support: {
        alt: "The trace list filtered to traces tagged email: agent traces with start time, name, duration and cost.",
        caption: "Every support conversation, with its cost and latency.",
      },
      coding: {
        alt: "The trace list filtered to traces with status error: failed runs, among them a coding-agent trace, with start time, name, duration and cost.",
        caption: "Only the failed runs, so nothing broken hides in the traffic.",
      },
      voice: {
        alt: "The trace list filtered to voice traces and sorted by duration, longest first: start time, name, duration and cost.",
        caption: "The slowest calls first, so you fix what callers feel.",
      },
      extraction: {
        alt: "The trace list filtered to traces tagged billing: start time, name, duration and cost, with extract-invoice among the rows.",
        caption: "Every billing trace in one list, with its duration and cost.",
      },
    },
  },
  {
    id: "explore",
    section: "Assistant",
    base: "marketing-assistant",
    label: "Assistant",
    stories: {
      support: {
        alt: "The assistant's window open over a dashboard: the question about answers that scored low, its answer, and the drafted dashboard of answer quality.",
        caption: "Ask which answers scored low, then get a quality dashboard built for you.",
      },
      coding: {
        alt: "The assistant's window open over a dashboard: the question about failed traces, an answer that names coding-agent, and the drafted dashboard of failed runs, error rate, cost per run and p95 duration.",
        caption: "Ask what failed, then get a dashboard for the coding agent built for you.",
      },
      voice: {
        alt: "The assistant's window open over a dashboard: the question about slow calls, its answer, and the drafted dashboard of voice latency.",
        caption: "Ask which calls were slow, then get a latency dashboard built for you.",
      },
      extraction: {
        alt: "The assistant's window open over a dashboard: the question about invoices that needed a retry, its answer, and the drafted dashboard of invoices, schema-check failures and cost.",
        caption: "Ask which invoices needed a retry, then get a dashboard built for you.",
      },
    },
  },
  {
    id: "issues",
    section: "Issues",
    base: "marketing-issue",
    label: "Issues",
    stories: {
      support: {
        alt: "An issue page for the question customers ask most, how to export traces to CSV: its state and actions, a daily chart of occurrences, and five example questions.",
        caption: "What customers ask most, grouped for you, with real examples.",
      },
      coding: {
        alt: "An issue page for run_tests failing with 1 failed and 47 passed: its state and actions, a daily chart of occurrences, and example traces.",
        caption: "The same test failure across runs becomes one issue to fix.",
      },
      voice: {
        alt: "An issue page for replies that started more than 800 ms after the caller: its state and actions, a daily chart of occurrences, and check_availability, which took 1.22 s, as its first example.",
        caption: "Slow replies grouped into one issue, with the trace that shows why.",
      },
      extraction: {
        alt: "An issue page for invoice totals that do not match their line items: its state and actions, a daily chart of occurrences, and an example with a total of 1,248.00 against line items of 1,284.00.",
        caption: "Wrong totals grouped into one issue, before they reach your books.",
      },
    },
  },
  {
    id: "evaluate",
    section: "Annotation queue",
    base: "marketing-queue-review",
    label: "Annotation queue",
    stories: {
      support: {
        alt: "An annotation queue's review form for support answers: the progress, the instructions and the correctness and relevance score fields.",
        caption: "Reviewers score real answers in a queue.",
      },
      coding: {
        alt: "An annotation queue's review form for patches: the progress, the instructions and the tests-pass and review-verdict fields.",
        caption: "Review the agent's patches the way you review a pull request.",
      },
      voice: {
        alt: "An annotation queue's review form for calls: the progress, the instructions and the resolved and tone fields.",
        caption: "Check whether each caller got what they asked for.",
      },
      extraction: {
        alt: "An annotation queue's review form for invoices: the progress, the instructions and the schema-valid and fields-correct fields.",
        caption: "Check the extracted fields against the invoice, one by one.",
      },
    },
  },
  {
    id: "prompts",
    section: "Playground",
    base: "marketing-playground",
    label: "Playground",
    stories: {
      support: {
        alt: "The playground with support-reply version 2: its messages, the variables filled in for a question about exporting traces to CSV, and the model's answer with its tokens, cost and latency.",
        caption: "Try a prompt change on a real question before you ship it.",
      },
      coding: {
        alt: "The playground with write-patch version 3: its messages, the variables filled in for a CSV bug, and the model's answer, a diff, with its tokens, cost and latency.",
        caption: "Run the patch prompt on a real issue and see the diff.",
      },
      voice: {
        alt: "The playground with voice-reply version 4: its messages, the caller's request to move an appointment, and the model's short spoken answer with its tokens, cost and latency.",
        caption: "Tune the receptionist prompt for short spoken answers.",
      },
      extraction: {
        alt: "The playground with extract-invoice version 5 in two columns on the same invoice: a cheaper model reads the total as 1248 and claude-sonnet-4-5 as 1284, each with its tokens, cost and latency.",
        caption: "Compare a cheap and a strong model on the same invoice, with the cost of each.",
      },
    },
  },
  {
    id: "alert",
    section: "Alerts",
    base: "marketing-alerts",
    label: "Alerts",
    stories: {
      support: {
        alt: "The alerts page with the rule for a correctness score under 0.7 on support-bot selected, with its condition and a preview chart.",
        caption: "Know when answer quality drops below your bar.",
      },
      coding: {
        alt: "The alerts page with the rule for an error rate above 5% on code-agent selected and firing, with its channel, Team Slack, and a preview chart.",
        caption: "Know when the coding agent's error rate climbs, in Slack.",
      },
      voice: {
        alt: "The alerts page with the rule for a p95 latency above 1.5 s on voice-agent selected and firing, with its condition and a preview chart.",
        caption: "Know when callers start waiting, before they hang up.",
      },
      extraction: {
        alt: "The alerts page with the rule for a schema-check pass rate under 97% on billing traces selected, with its condition and a preview chart.",
        caption: "Know when the schema check pass rate slips.",
      },
    },
  },
];

/** The shot's name in screenshots.json, e.g. `shotName("hero", "voice")` is "marketing-trace-view-voice". */
export function shotName(pictureId, storyId) {
  const picture = pictureById(pictureId);
  const story = STORIES.find((s) => s.id === storyId);
  if (!story) throw new Error(`stories: unknown story "${storyId}"`);
  // The first story keeps the bare name; the others add their id.
  const suffix = story.id === STORIES[0].id ? "" : `-${story.id}`;
  return `${picture.base}${suffix}`;
}

/** @returns {Picture} */
export function pictureById(pictureId) {
  const picture = PICTURES.find((p) => p.id === pictureId);
  if (!picture) throw new Error(`stories: unknown picture "${pictureId}"`);
  return picture;
}

/**
 * The four panels of one picture, in STORIES order.
 * @returns {{ story: string, shot: string, label: string, alt: string, caption: string }[]}
 */
export function panelsOf(pictureId) {
  const picture = pictureById(pictureId);
  return STORIES.map(({ id }) => {
    const content = picture.stories[id];
    if (!content) throw new Error(`stories: picture "${pictureId}" has no story "${id}"`);
    const label = typeof picture.label === "string" ? picture.label : picture.label[id];
    return { story: id, shot: shotName(pictureId, id), label, ...content };
  });
}
