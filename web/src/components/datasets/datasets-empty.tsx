import { Link } from "@tanstack/react-router";
import { Code, Database, type LucideIcon, Plus, Upload, Waypoints } from "lucide-react";
import type { ReactNode } from "react";
import { DisabledReason } from "@/components/disabled-reason";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";

const CODE_SNIPPET = `import cetra

client = cetra.Client()
ds = client.datasets.create("faq-v1", description="FAQ questions")
ds.add_items([{"input": {"q": "…"}, "expected_output": {"a": "…"}}])
`;

/**
 * The empty dataset list: how to create a dataset. `onCreate` opens the new-dataset dialog;
 * `canCreate` false (viewers) disables the button with the reason.
 */
export function DatasetsListEmpty({
  onCreate,
  canCreate = true,
}: {
  onCreate: () => void;
  canCreate?: boolean;
}) {
  return (
    <div className="flex flex-col items-center gap-4 px-4 py-16 text-center">
      <Database className="size-8 text-muted-foreground" aria-hidden />
      <h2 className="text-title font-medium">Create your first dataset</h2>
      <p className="max-w-md text-ui text-muted-foreground">
        A dataset is a set of inputs, with the outputs you expect, that you run your app against.
      </p>
      <div className="grid w-full max-w-3xl gap-3 text-left sm:grid-cols-3">
        <HowToCard icon={Plus} title="Create">
          <p className="text-muted-foreground">Start an empty dataset and add items to it.</p>
          <DisabledReason reason={canCreate ? undefined : "Only members can create datasets"}>
            <Button
              type="button"
              variant="outline"
              size="sm"
              className="w-fit"
              disabled={!canCreate}
              onClick={onCreate}
            >
              Create dataset
            </Button>
          </DisabledReason>
        </HowToCard>
        <HowToCard icon={Waypoints} title="From a trace">
          <p className="text-muted-foreground">
            Open a trace and use “Add to dataset” to keep its input and output.
          </p>
          <TracesLink />
        </HowToCard>
        <HowToCard icon={Code} title="From code">
          <pre className="overflow-x-auto rounded-md border bg-muted p-2 text-left font-mono text-caption">
            <code>{CODE_SNIPPET}</code>
          </pre>
        </HowToCard>
      </div>
    </div>
  );
}

/** The empty item table of the latest version: how to add items. */
export function DatasetItemsEmpty({
  onAddItem,
  onImport,
}: {
  onAddItem: () => void;
  onImport: () => void;
}) {
  return (
    <div className="flex flex-col items-center gap-4 px-4 py-16 text-center">
      <Database className="size-8 text-muted-foreground" aria-hidden />
      <h2 className="text-title font-medium">No items yet</h2>
      <div className="grid w-full max-w-3xl gap-3 text-left sm:grid-cols-3">
        <HowToCard icon={Plus} title="Add item">
          <p className="text-muted-foreground">Write one item by hand as JSON.</p>
          <Button type="button" variant="outline" size="sm" className="w-fit" onClick={onAddItem}>
            Add item
          </Button>
        </HowToCard>
        <HowToCard icon={Upload} title="Import a file">
          <p className="text-muted-foreground">Import a CSV or JSONL file, and map its columns.</p>
          <Button type="button" variant="outline" size="sm" className="w-fit" onClick={onImport}>
            Import a file
          </Button>
        </HowToCard>
        <HowToCard icon={Waypoints} title="From a trace">
          <p className="text-muted-foreground">
            Open a trace and use “Add to dataset” to keep its input and output.
          </p>
          <TracesLink />
        </HowToCard>
      </div>
    </div>
  );
}

function HowToCard({
  icon: Icon,
  title,
  children,
}: {
  icon: LucideIcon;
  title: string;
  children: ReactNode;
}) {
  return (
    <section className="flex min-w-0 flex-1 basis-56 flex-col gap-2 rounded-md border p-3 text-body">
      <h3 className="flex items-center gap-1.5 text-ui font-medium">
        <Icon className="size-3.5 text-muted-foreground" aria-hidden />
        {title}
      </h3>
      {children}
    </section>
  );
}

function TracesLink() {
  const { org, project } = useProject();
  return (
    <Link
      to="/$org/$project/traces"
      params={{ org, project }}
      className="w-fit text-body underline-offset-4 hover:underline"
    >
      Go to traces
    </Link>
  );
}
