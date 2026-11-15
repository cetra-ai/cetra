import { type FormEvent, useState } from "react";
import { $api } from "@/api/client";
import { problemMessage } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { FormError } from "@/components/form/form-error";
import { TextField } from "@/components/form/text-field";
import { Button } from "@/components/ui/button";
import { formatUnitPrice } from "@/lib/prices";

const SCOPE_LABELS = { project: "project", org: "organization", builtin: "built-in" } as const;

/** "Which price applies?": asks the server which row prices a model now (D-T-7). */
export function PriceResolver({ org, project }: { org: string; project: string }) {
  const [model, setModel] = useState("");
  const [provider, setProvider] = useState("");
  const [modelError, setModelError] = useState<string | undefined>();
  const [submitted, setSubmitted] = useState<{ model: string; provider: string } | undefined>();
  const query = $api.useQuery(
    "get",
    "/api/v1/projects/{project}/models/resolve",
    {
      params: {
        path: { project: projectRef(org, project) },
        query: {
          model: submitted?.model ?? "",
          ...(submitted?.provider ? { provider: submitted.provider } : {}),
        },
      },
    },
    { enabled: submitted !== undefined },
  );

  function onSubmit(event: FormEvent) {
    event.preventDefault();
    const name = model.trim();
    if (name === "") {
      setModelError("Enter a model name.");
      return;
    }
    setModelError(undefined);
    setSubmitted({ model: name, provider: provider.trim() });
  }

  const price = query.data?.price;

  return (
    <form noValidate className="flex flex-col gap-4" onSubmit={onSubmit}>
      <div className="grid gap-4 sm:grid-cols-2">
        <TextField
          id="resolve-model"
          label="Model"
          value={model}
          error={modelError}
          onChange={setModel}
        />
        <TextField
          id="resolve-provider"
          label="Provider"
          value={provider}
          onChange={setProvider}
          description="Optional, e.g. openai"
        />
      </div>
      <div>
        <Button type="submit">Check</Button>
      </div>
      {query.error ? (
        <FormError message={problemMessage(query.error, "Could not check the price.")} />
      ) : null}
      {submitted !== undefined && query.data !== undefined ? (
        <p role="status" className="text-sm">
          {price === undefined
            ? "No price matches this model. Its spans get no cost, and a budgeted online rule cannot use it as a judge."
            : `Priced by the ${SCOPE_LABELS[price.scope]} price “${price.model_name}”: input ${formatUnitPrice(price.price_input)}, output ${formatUnitPrice(price.price_output)} per 1M tokens.`}
        </p>
      ) : null}
    </form>
  );
}
