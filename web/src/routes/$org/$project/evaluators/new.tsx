import { createFileRoute, Link, useNavigate, useRouter } from "@tanstack/react-router";
import { ChevronLeft } from "lucide-react";
import { BACK_LINK_CLASS, BackLink } from "@/components/back-link";
import { EvaluatorForm } from "@/components/evaluators/evaluator-form";
import { useProject } from "@/components/shell/project-context";
import { useBackTo } from "@/lib/list-return";
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
  const router = useRouter();
  const backTo = useBackTo();
  return (
    <div className="mx-auto flex w-full max-w-5xl flex-1 flex-col gap-6 overflow-auto px-4 py-6">
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
        <h2 className="text-title font-medium">New evaluator</h2>
      </div>
      <EvaluatorForm
        org={org}
        project={project}
        initialTemplateId={search.template}
        onCancel={() => {
          // Cancel leaves the way the back link does: to where the page was opened from.
          if (backTo) {
            router.history.push(backTo.href);
          } else {
            void navigate({ to: "/$org/$project/evaluators", params: { org, project } });
          }
        }}
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
