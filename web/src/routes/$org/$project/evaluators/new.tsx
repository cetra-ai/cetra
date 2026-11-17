import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import { ChevronLeft } from "lucide-react";
import { BACK_LINK_CLASS, BackLink } from "@/components/back-link";
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
      <div className="flex min-w-0 flex-wrap items-center gap-2">
        <BackLink
          fallback={
            <Link
              to="/$org/$project/evaluators"
              params={{ org, project }}
              aria-label="Back to evaluators"
              className={BACK_LINK_CLASS}
            >
              <ChevronLeft aria-hidden="true" className="size-3.5" />
              Evaluators
            </Link>
          }
        />
        <span aria-hidden="true" className="text-muted-foreground/60">
          /
        </span>
        <h1 className="text-title font-medium">New evaluator</h1>
      </div>
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
