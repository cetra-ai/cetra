import { createFileRoute, Link, useNavigate, useRouter } from "@tanstack/react-router";
import { cn } from "cn";
import { ChevronLeft } from "lucide-react";
import { useEffect, useState } from "react";

import { projectRef } from "@/api/project-ref";
import { BACK_LINK_CLASS, BackLink } from "@/components/back-link";
import { EvaluatorForm } from "@/components/evaluators/evaluator-form";
import { ITEM_TITLE_CLASS, PageToolbar, PageToolbarTitle } from "@/components/page-toolbar";

import { useProject } from "@/components/shell/project-context";
import { subscribeDrafts, takeDraft } from "@/lib/assistant/handoff";
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
  const ref = projectRef(org, project);
  // An assistant's draft, taken when the page opens or when one is handed over while it is shown
  // (D-AS-28). A new draft remounts the form with it: the key changes.
  const [draft, setDraft] = useState(() => takeDraft(ref, "evaluator"));
  const [formKey, setFormKey] = useState(0);
  useEffect(
    () =>
      subscribeDrafts(ref, () => {
        const next = takeDraft(ref, "evaluator");
        if (next !== undefined) {
          setDraft(next);
          setFormKey((key) => key + 1);
        }
      }),
    [ref],
  );
  return (
    <div className="flex h-full min-h-0 w-full flex-col">
      <PageToolbar>
        <PageToolbarTitle
          back={
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
          }
          label="Evaluator"
        >
          <h2 className={ITEM_TITLE_CLASS}>New evaluator</h2>
        </PageToolbarTitle>
      </PageToolbar>
      <div className="min-h-0 flex-1 overflow-auto">
        <div className={cn("flex flex-col gap-6 px-3 py-3", "max-w-3xl")}>
          <EvaluatorForm
            key={formKey}
            org={org}
            project={project}
            initialTemplateId={search.template}
            initialDraft={draft}
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
      </div>
    </div>
  );
}
