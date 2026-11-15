import { createFileRoute } from "@tanstack/react-router";
import { RuleForm } from "@/components/online-rules/rule-form";
import { useProject } from "@/components/shell/project-context";

export const Route = createFileRoute("/$org/$project/online-rules/new")({
  component: NewOnlineRulePage,
});

function NewOnlineRulePage() {
  const { org, project } = useProject();
  return (
    <div className="mx-auto flex w-full max-w-3xl flex-1 flex-col gap-6 overflow-auto px-4 py-6">
      <h1 className="text-lg font-medium">New rule</h1>
      <RuleForm org={org} project={project} />
    </div>
  );
}
