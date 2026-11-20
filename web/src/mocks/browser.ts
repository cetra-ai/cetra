import { setupWorker } from "msw/browser";
import { dashboardHandlers, seedDashboards } from "./dashboards";
import { handlers } from "./handlers";
import { playgroundHandlers } from "./playground";
import { savedViewHandlers, seedSavedViews } from "./saved-views";
import { sqlHandlers } from "./sql";
import { fixtureStreamHandlers } from "./sse";
import { writeHandlers } from "./writes";

export const worker = setupWorker(
  ...fixtureStreamHandlers(),
  ...playgroundHandlers,
  ...savedViewHandlers(seedSavedViews()),
  ...dashboardHandlers(seedDashboards()),
  ...sqlHandlers,
  ...writeHandlers,
  ...handlers,
);
