import { createFileRoute, Link } from "@tanstack/react-router";
import { ChevronLeft } from "lucide-react";
import { useEffect, useState } from "react";
import { projectRef } from "@/api/project-ref";
import { BACK_LINK_CLASS, BackLink } from "@/components/back-link";
import { RuleForm } from "@/components/online-rules/rule-form";
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
    <div className="mx-auto flex w-full max-w-3xl flex-1 flex-col gap-6 overflow-auto px-4 py-6">
      <div className="flex min-w-0 flex-wrap items-center gap-2">
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
        <span aria-hidden="true" className="text-muted-foreground/60">
          /
        </span>
        <h2 className="text-title font-medium">New rule</h2>
      </div>
      <RuleForm key={formKey} org={org} project={project} initialDraft={draft} />
    </div>
  );
}
