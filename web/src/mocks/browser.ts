import { setupWorker } from "msw/browser";
import { assistantHandlers } from "./assistant";
import { dashboardHandlers, seedDashboards } from "./dashboards";
import { handlers } from "./handlers";
import { savedViewHandlers, seedSavedViews } from "./saved-views";
import { sqlHandlers } from "./sql";
import { fixtureStreamHandlers } from "./sse";
import { writeHandlers } from "./writes";

export const worker = setupWorker(
  ...fixtureStreamHandlers(),
  ...savedViewHandlers(seedSavedViews()),
  ...dashboardHandlers(seedDashboards()),
  ...sqlHandlers,
  ...assistantHandlers(),
  ...writeHandlers,
  ...handlers,
);
