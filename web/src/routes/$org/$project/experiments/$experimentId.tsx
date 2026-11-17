import { createFileRoute, redirect } from "@tanstack/react-router";
import { parseExperimentSearch } from "@/lib/experiments";

/** Experiment links land here: the list opens the experiment in its panel, with `errors` and `item`. */
export const Route = createFileRoute("/$org/$project/experiments/$experimentId")({
  validateSearch: (search) => parseExperimentSearch(search),
  beforeLoad: ({ params, search }) => {
    throw redirect({
      to: "/$org/$project/experiments",
      params: { org: params.org, project: params.project },
      search: { ...search, experiment: params.experimentId },
      replace: true,
    });
  },
});
