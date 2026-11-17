import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { EvaluatorForm } from "@/components/evaluators/evaluator-form";
import { useProject } from "@/components/shell/project-context";
import { parseString } from "@/lib/search/common";

const TEMPLATE_ID_MAX = 64;

export const Route = createFileRoute("/$org/$project/evaluators/new")({
  validateSearch: (search: Record<string, unknown>): { template?: string } => {
    const template = parseString(search.template, TEMPLATE_ID_MAX);
    return template ? { template } : {};
  },
  component: NewEvaluatorPage,
});

function NewEvaluatorPage() {
  const { org, project } = useProject();
  const navigate = useNavigate();
  const search = Route.useSearch();
  return (
    <div className="mx-auto flex w-full max-w-3xl flex-1 flex-col gap-6 overflow-auto px-4 py-6">
      <h1 className="text-lg font-medium">New evaluator</h1>
      <EvaluatorForm
        org={org}
        project={project}
        initialTemplateId={search.template}
        onSaved={(saved) => {
          void navigate({
            to: "/$org/$project/evaluators/$evaluatorId",
            params: { org, project, evaluatorId: saved.id },
          });
        }}
      />
    </div>
  );
}
