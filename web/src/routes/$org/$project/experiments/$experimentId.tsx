import { createFileRoute, Link, notFound, useNavigate } from "@tanstack/react-router";
import { problemOf } from "@/api/problem";
import { experimentQuery } from "@/api/queries-experiments";
import { ExperimentDetail } from "@/components/experiments/experiment-detail";
import { useProject } from "@/components/shell/project-context";
import { parseExperimentSearch } from "@/lib/experiments";

export const Route = createFileRoute("/$org/$project/experiments/$experimentId")({
  validateSearch: (search) => parseExperimentSearch(search),
  loader: async ({ context, params }) => {
    try {
      await context.queryClient.ensureQueryData(
        experimentQuery(params.org, params.project, params.experimentId),
      );
    } catch (error) {
      if (problemOf(error)?.status === 404) {
        throw notFound();
      }
      throw error;
    }
  },
  component: ExperimentPage,
});

/** The experiment's full page. The errors switch and the open item live in the URL. */
function ExperimentPage() {
  const { org, project } = useProject();
  const { experimentId } = Route.useParams();
  const search = Route.useSearch();
  const navigate = useNavigate();
  return (
    <div className="flex w-full flex-1 flex-col gap-6 overflow-auto px-4 py-4">
      <Link
        to="/$org/$project/experiments"
        params={{ org, project }}
        search={{}}
        className="w-fit rounded-sm text-body text-muted-foreground underline-offset-4 outline-none hover:underline focus-visible:outline-2 focus-visible:outline-ring focus-visible:outline-offset-2 focus-visible:outline-solid"
      >
        Experiments
      </Link>
      <ExperimentDetail
        experimentId={experimentId}
        errorsOnly={search.errors === 1}
        onErrorsOnlyChange={(checked) => {
          void navigate({
            to: "/$org/$project/experiments/$experimentId",
            params: { org, project, experimentId },
            search: (prev) => ({ ...prev, errors: checked ? 1 : undefined }),
          });
        }}
        item={search.item}
        onItemChange={(item) => {
          void navigate({
            to: "/$org/$project/experiments/$experimentId",
            params: { org, project, experimentId },
            search: (prev) => ({ ...prev, item }),
            replace: false,
          });
        }}
      />
    </div>
  );
}
