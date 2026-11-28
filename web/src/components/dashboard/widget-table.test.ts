import { expect, it } from "vitest";
import { groupHeading } from "@/components/dashboard/widget-table";

it("names the group column after the breakdown field, a map field by its key", () => {
  expect(groupHeading("release")).toBe("release");
  expect(groupHeading("metadata['repository']")).toBe("repository");
  expect(groupHeading("attributes['gen_ai.system']")).toBe("gen_ai.system");
  expect(groupHeading("resource_attributes['service.version']")).toBe("service.version");
  expect(groupHeading(undefined)).toBe("Group");
});
