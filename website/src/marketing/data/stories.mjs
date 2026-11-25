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
      coding: "Traces · Code agent",
      voice: "Traces · Voice desk",
      extraction: "Traces · Invoice pipeline",
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
    section: "Trace",
    base: "marketing-trace-page",
    label: {
      support: "Session · Support bot",
      coding: "Trace · Code agent",
      voice: "Trace · Voice desk",
      extraction: "Traces · Invoice pipeline",
    },
    stories: {
      support: {
        alt: "The session view of a support conversation: four turns with their scores and costs, the third asking whether the agent can schedule a weekly visit and scoring 0.3 on correctness, with a failed user-feedback mark.",
        caption: "Each turn of a session, with its score, so the bad answer stands out.",
      },
      coding: {
        alt: "A coding agent's transcript: the CSV fix request, the agent reading and searching the code, a run_tests error of 1 failed and 47 passed, and a final answer that reports 48 tests passing.",
        caption: "Read the code, patch it and rerun the tests, all in one transcript.",
      },
      voice: {
        alt: "A voice agent's draft-reply span: the caller's recording with its transcript, the calendar check and its slots at 9:30 and 11:00, and the agent's spoken reply, with its transcript.",
        caption: "Hear the caller and the agent, and read what was said, in one voice turn.",
      },
      extraction: {
        alt: "The trace list filtered to the batch tagged batch-1119: 200 invoice traces, with the three failed ones at the top marked by a red edge, and the totals line reading 200 traces, 3 errors and $0.48.",
        caption: "A 200-invoice batch, with its three failed invoices at the top.",
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
        alt: "The assistant's window over the traces page: the question about thumbs-down answers, its answer that 62% quote the old pricing page, and a filter card with Open in traces.",
        caption: "Ask why answers got a thumbs-down, then open the traces behind them.",
      },
      coding: {
        alt: "The assistant's window over the traces page: the question about cost per run, its answer that 18% of runs call run_tests more than five times and make 70% of the cost, and a filter card with Open in traces.",
        caption: "Ask why runs cost more, then open the retried runs.",
      },
      voice: {
        alt: "The assistant's window over the traces page: the question about hang-ups, its answer that 71% follow a turn over 2.5 s on check_availability, and a filter card with Open in traces.",
        caption: "Ask why callers hang up, then open the slow calendar checks.",
      },
      extraction: {
        alt: "The assistant's window over the traces page: the question about failing vendors, its answer that 38% of Nordic Freight's invoices were re-extracted, all on total, and a filter card with Open in traces.",
        caption: "Ask which vendors fail, then open the re-extracted invoices.",
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
        alt: "An issue page titled Customers asking for SSO on the Starter plan: 244 occurrences in 14 days, a daily chart, and five examples asking whether SSO comes with the Starter plan.",
        caption: "The same question, 244 times in 14 days, grouped into one issue.",
      },
      coding: {
        alt: "An issue page titled ContextWindowExceeded: diff over 23 files: 70 occurrences in 14 days, 12 users on the busiest day, and an example saying the diff was too large for the model's context.",
        caption: "Runs that stop when a diff no longer fits, grouped into one issue.",
      },
      voice: {
        alt: "An issue page titled Agent talked over the caller: 11 occurrences in 14 days, 7 users on the busiest day, and an example whose interruption check scored 0.3.",
        caption: "Replies that start before the caller has finished, grouped into one issue.",
      },
      extraction: {
        alt: "An issue page titled Scanned PDF: OCR returned no text: 54 occurrences in 14 days, and an example saying the PDF is a scanned image with no text layer.",
        caption: "Scanned invoices that give OCR nothing to read, grouped into one issue.",
      },
    },
  },
  {
    id: "evaluate",
    section: "Evaluate",
    base: "marketing-evaluate",
    label: {
      support: "Annotation queue",
      coding: "Online rule",
      voice: "LLM judge",
      extraction: "Experiments",
    },
    stories: {
      support: {
        alt: "An annotation queue's review of an answer that got a thumbs-down: the progress reads 4 of 25 done, and the form has correctness, tone and policy fields with their instructions.",
        caption: "Reviewers score each flagged answer in a queue, one by one.",
      },
      coding: {
        alt: "The scores chart of an online rule over the last 24 hours, headed 66% with 411 scores, drawn as a line from 24 hours ago to now.",
        caption: "The rule's scores over 24 hours, so a drop shows at a glance.",
      },
      voice: {
        alt: "The LLM judge Reads the booking back on its Configuration tab: its question about the booking, a test on one trace, and the result false, with the reason that the agent never read the booking back.",
        caption: "Write the judge's question once, then test it on a real call.",
      },
      extraction: {
        alt: "The comparison of extract-invoice versions 3 and 4 on 300 invoices: total passes 99% against 97%, vat_number 96% against 91%, and cost $0.72 against $0.63.",
        caption: "Version 4 lifts vat_number from 91% to 96% on 300 invoices.",
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
        alt: "The playground with support-reply versions 2 and 3 side by side on a question about a weekly schedule: version 2 says you can export any time, and version 3 says cetra can't schedule exports yet and offers the export by hand.",
        caption: "Run two prompt versions on the same question, and see which one answers right.",
      },
      coding: {
        alt: "The playground with write-patch version 4: its two messages, the variables for a rename across 23 files, and the model's reply, three steps for the change.",
        caption: "Run the patch prompt on a wide change, and see it split into steps.",
      },
      voice: {
        alt: "The playground with voice-reply version 5: the receptionist prompt, the caller's request to move an appointment to Friday at 9:30 with Dr. Martin, and the reply Done: Friday at 9:30, with Dr. Martin.",
        caption: "Tune the receptionist prompt so it reads the booking back.",
      },
      extraction: {
        alt: "The playground with extract-invoice version 6: the system prompt on comma decimals, the OCR text of a Nordic Freight invoice, and the JSON reply with total 1234.56 in EUR.",
        caption: "A comma-decimal total, 1.234,56, comes back as 1234.56 in JSON.",
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
        alt: "The alerts page with the rule Answer correctness under 0.7 selected: firing for 5 hours, last value 0.517, condition score under 0.700 over 1 hour, Team Slack, and a preview chart crossing its threshold.",
        caption: "Know when answer quality drops below your bar.",
      },
      coding: {
        alt: "The alerts page with the rule Spend over $40 an hour selected: firing for 14 hours, last value $45.07, condition cost over $40.00 over 1 hour, Team Slack, and a preview chart above its threshold.",
        caption: "Know when an hour of agent spend goes over your budget.",
      },
      voice: {
        alt: "The alerts page with the rule Slow replies on the voice desk selected: firing for 6 hours, last value 3550 ms, condition p95 latency over 1500 ms over 15 minutes, On-call pager, and a preview chart.",
        caption: "Know when one in twenty replies takes longer than 1.5 s.",
      },
      extraction: {
        alt: "The alerts page with the rule Scanned PDFs over 10 an hour selected: firing for 8 minutes, last value 14, condition issue occurrences over 10 in 1 hour, Team Slack, and a 24-hour preview chart that rises above its threshold in the last hour.",
        caption: "Know when scanned invoices come in faster than 10 an hour.",
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
