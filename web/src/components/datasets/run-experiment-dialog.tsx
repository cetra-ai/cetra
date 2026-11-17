import { Link } from "@tanstack/react-router";
import { EvaluateSnippet } from "@/components/experiments/evaluate-snippet";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";

/** A "Run experiment" button that opens the code to run an experiment on a dataset. */
export function RunExperimentDialog({
  datasetId,
  datasetName,
}: {
  datasetId: string;
  datasetName: string;
}) {
  const { org, project } = useProject();
  return (
    <Dialog>
      <DialogTrigger asChild>
        <Button type="button" variant="outline" size="sm">
          Run experiment
        </Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle className="text-title">{`Run an experiment on ${datasetName}`}</DialogTitle>
          <DialogDescription className="text-ui">
            {"Experiments run from your code with "}
            <code className="font-mono">evaluate()</code>
            {"; results appear here."}
          </DialogDescription>
        </DialogHeader>
        <EvaluateSnippet datasetName={datasetName} />
        <Link
          to="/$org/$project/datasets/$datasetId"
          params={{ org, project, datasetId }}
          search={{ tab: "experiments" }}
          className="w-fit text-body underline-offset-4 hover:underline"
        >
          Experiments of this dataset
        </Link>
      </DialogContent>
    </Dialog>
  );
}
