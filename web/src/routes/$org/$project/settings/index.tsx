import { createFileRoute } from "@tanstack/react-router";
import { ProjectSettingsHome } from "@/components/settings/settings-home";

export const Route = createFileRoute("/$org/$project/settings/")({
  component: ProjectSettingsHome,
});
