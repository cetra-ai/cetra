import { HttpResponse, http } from "msw";
import previewDocument from "../../../fixtures/api/static/online-rule-preview.json";

/** The preview every form test gets; a test that needs another answer overrides it with `server.use`. */
export function onlineRulePreviewHandler() {
  return http.post("*/api/v1/projects/:project/online-rules/preview", () =>
    HttpResponse.json(previewDocument),
  );
}
