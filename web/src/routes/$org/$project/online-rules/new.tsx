import { createFileRoute, Link } from "@tanstack/react-router";
import { cn } from "cn";
import { ChevronLeft } from "lucide-react";
import { useEffect, useState } from "react";

import { projectRef } from "@/api/project-ref";
import { BACK_LINK_CLASS, BackLink } from "@/components/back-link";
import { RuleForm } from "@/components/online-rules/rule-form";
import { ITEM_TITLE_CLASS, PageToolbar, PageToolbarTitle } from "@/components/page-toolbar";

import { useProject } from "@/components/shell/project-context";
import { subscribeDrafts, takeDraft } from "@/lib/assistant/handoff";

export const Route = createFileRoute("/$org/$project/online-rules/new")({
  component: NewOnlineRulePage,
});

function NewOnlineRulePage() {
  const { org, project } = useProject();
  const ref = projectRef(org, project);
  // An assistant's draft, taken when the page opens or when one is handed over while it is shown
  // (D-AS-28). A new draft remounts the form with it: the key changes.
  const [draft, setDraft] = useState(() => takeDraft(ref, "online_rule"));
  const [formKey, setFormKey] = useState(0);
  useEffect(
    () =>
      subscribeDrafts(ref, () => {
        const next = takeDraft(ref, "online_rule");
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
                  to="/$org/$project/online-rules"
                  params={{ org, project }}
                  search={{}}
                  aria-label="Back to online rules"
                  className={BACK_LINK_CLASS}
                >
                  <ChevronLeft aria-hidden="true" className="size-3.5" />
                  Online rules
                </Link>
              }
            />
          }
          label="Online rule"
        >
          <h2 className={ITEM_TITLE_CLASS}>New rule</h2>
        </PageToolbarTitle>
      </PageToolbar>
      <div className="min-h-0 flex-1 overflow-auto">
        <div className={cn("flex flex-col gap-6 px-3 py-3", "max-w-3xl")}>
          <RuleForm key={formKey} org={org} project={project} initialDraft={draft} />
        </div>
      </div>
    </div>
  );
}
