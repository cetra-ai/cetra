import { Link } from "@tanstack/react-router";
import { $api } from "@/api/client";
import { evaluatorTemplatesInit } from "@/api/queries-evaluation";
import type { Schema } from "@/api/types";
import { Skeleton } from "@/components/ui/skeleton";
import { useBackState } from "@/lib/list-return";

type Template = Schema<"EvaluatorTemplate">;

/** The output type a template's judge produces: `numeric`, `boolean` or `categorical`. */
function outputTypeOf(template: Template): string {
  const type: unknown = template.output.type;
  return typeof type === "string" ? type : "—";
}

/** "Start from a template": one card per template, each opening the new-evaluator form with it. */
export function TemplateGallery({ org, project }: { org: string; project: string }) {
  // The new evaluator page's back link returns to the list the gallery is on.
  const backState = useBackState("Evaluators");
  const templates = $api.useQuery(
    "get",
    "/api/v1/projects/{project}/evaluator-templates",
    evaluatorTemplatesInit(org, project),
  );

  if (templates.isPending) {
    return <Skeleton className="h-20 w-full" />;
  }
  const list = templates.data?.data ?? [];
  if (templates.isError || list.length === 0) {
    return null;
  }

  return (
    <section aria-labelledby="template-gallery-heading" className="flex flex-col gap-3">
      <h2 id="template-gallery-heading" className="text-body font-medium">
        Start from a template
      </h2>
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3">
        {list.map((template) => (
          <Link
            key={template.id}
            to="/$org/$project/evaluators/new"
            params={{ org, project }}
            search={{ template: template.id }}
            state={backState}
            className="flex flex-col gap-1 rounded-md border bg-card p-3 text-card-foreground outline-none hover:bg-muted/50 focus-visible:outline-2 focus-visible:outline-ring focus-visible:outline-offset-2 focus-visible:outline-solid"
          >
            <span className="flex items-center justify-between gap-2">
              <span className="text-ui font-medium">{template.name}</span>
              <span className="text-caption text-muted-foreground">{outputTypeOf(template)}</span>
            </span>
            <span className="line-clamp-1 text-body text-muted-foreground">
              {template.description}
            </span>
          </Link>
        ))}
      </div>
    </section>
  );
}
