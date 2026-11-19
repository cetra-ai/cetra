import { useState } from "react";
import {
  isStaleDashboard,
  useCreateDashboard,
  useDeleteDashboard,
  useResetOverview,
  useUpdateDashboard,
} from "@/api/dashboards";
import { fieldErrors, problemMessage } from "@/api/problem";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import { Textarea } from "@/components/ui/textarea";
import { TEMPLATES, type TemplateId } from "@/lib/dashboards/templates";
import type { Dashboard, Definition } from "@/lib/dashboards/types";
import { validateDashboard } from "@/lib/dashboards/validate";

const STALE_MESSAGE = "Someone else changed this dashboard. Reload it and try again.";

/** The choices under "Start from": a blank dashboard, then the templates. */
const STARTS: readonly { id: "blank" | TemplateId; name: string; description: string }[] = [
  { id: "blank", name: "Blank", description: "An empty dashboard." },
  ...TEMPLATES.map((template) => ({
    id: template.id,
    name: template.name,
    description: template.description,
  })),
];

/** The field and form messages of a failed save. Server paths map to `name` and `description`. */
type Failure = { name?: string; description?: string; form?: string };

function failureOf(error: unknown, stale: boolean): Failure {
  if (stale) {
    return { form: STALE_MESSAGE };
  }
  const fields = fieldErrors(error);
  const other = Object.keys(fields).some((key) => key !== "name" && key !== "description");
  const hasNamed = fields.name !== undefined || fields.description !== undefined;
  return {
    name: fields.name,
    description: fields.description,
    form: other || !hasNamed ? problemMessage(error, "Could not save the dashboard.") : undefined,
  };
}

/** The name and description of a form, with the client rules shown once the form was submitted. */
function clientIssues(name: string, description: string, submitted: boolean) {
  const issues = validateDashboard({ name, description });
  return {
    name: submitted ? issues.find((issue) => issue.field === "name")?.message : undefined,
    description: submitted
      ? issues.find((issue) => issue.field === "description")?.message
      : undefined,
    valid: issues.length === 0,
  };
}

function DashboardFields({
  name,
  description,
  onName,
  onDescription,
  failure,
  issues,
}: {
  name: string;
  description: string;
  onName: (value: string) => void;
  onDescription: (value: string) => void;
  failure: Failure;
  issues: ReturnType<typeof clientIssues>;
}) {
  const nameError = issues.name ?? failure.name;
  const descriptionError = issues.description ?? failure.description;
  return (
    <>
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="dashboard-name">Name</Label>
        <Input
          id="dashboard-name"
          value={name}
          aria-invalid={nameError !== undefined}
          onChange={(event) => onName(event.target.value)}
        />
        {nameError ? <p className="text-caption text-destructive">{nameError}</p> : null}
      </div>
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="dashboard-description">Description (optional)</Label>
        <Textarea
          id="dashboard-description"
          value={description}
          aria-invalid={descriptionError !== undefined}
          onChange={(event) => onDescription(event.target.value)}
        />
        {descriptionError ? (
          <p className="text-caption text-destructive">{descriptionError}</p>
        ) : null}
      </div>
    </>
  );
}

/** Creates an empty dashboard, then calls `onCreated` with it. */
export function NewDashboardDialog({
  org,
  project,
  open,
  onOpenChange,
  onCreated,
}: {
  org: string;
  project: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onCreated: (dashboard: Dashboard) => void;
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <NewForm
          org={org}
          project={project}
          onCancel={() => onOpenChange(false)}
          onCreated={onCreated}
        />
      </DialogContent>
    </Dialog>
  );
}

function NewForm({
  org,
  project,
  onCancel,
  onCreated,
}: {
  org: string;
  project: string;
  onCancel: () => void;
  onCreated: (dashboard: Dashboard) => void;
}) {
  const create = useCreateDashboard(org, project);
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [start, setStart] = useState<"blank" | TemplateId>("blank");
  const [submitted, setSubmitted] = useState(false);
  const failure = failureOf(create.error, false);
  const issues = clientIssues(name, description, submitted);

  async function submit(): Promise<void> {
    setSubmitted(true);
    if (!issues.valid) {
      return;
    }
    const definition =
      start === "blank"
        ? { widgets: [] }
        : (TEMPLATES.find((template) => template.id === start)?.definition ?? { widgets: [] });
    const dashboard = await create
      .mutateAsync({ name, description, definition })
      .catch(() => undefined);
    if (dashboard) {
      onCreated(dashboard);
    }
  }

  return (
    <form
      className="contents"
      onSubmit={(event) => {
        event.preventDefault();
        void submit();
      }}
    >
      <DialogHeader>
        <DialogTitle>New dashboard</DialogTitle>
        <DialogDescription>Create a dashboard and open it.</DialogDescription>
      </DialogHeader>
      <DashboardFields
        name={name}
        description={description}
        onName={setName}
        onDescription={setDescription}
        failure={failure}
        issues={issues}
      />
      <div className="flex flex-col gap-2">
        <span id="dashboard-start-label" className="text-caption text-muted-foreground">
          Start from
        </span>
        <RadioGroup
          aria-labelledby="dashboard-start-label"
          value={start}
          onValueChange={(value) => {
            const choice = STARTS.find((item) => item.id === value);
            if (choice) {
              setStart(choice.id);
            }
          }}
          disabled={create.isPending}
        >
          {STARTS.map((choice) => (
            <div key={choice.id} className="flex items-start gap-2">
              <RadioGroupItem
                value={choice.id}
                id={`dashboard-start-${choice.id}`}
                aria-label={choice.name}
                className="mt-1"
              />
              <Label htmlFor={`dashboard-start-${choice.id}`} className="flex flex-col gap-0.5">
                <span className="text-ui font-medium">{choice.name}</span>
                <span className="text-caption text-muted-foreground">{choice.description}</span>
              </Label>
            </div>
          ))}
        </RadioGroup>
      </div>
      {failure.form ? <p className="text-caption text-destructive">{failure.form}</p> : null}
      <DialogFooter>
        <Button type="button" variant="outline" onClick={onCancel}>
          Cancel
        </Button>
        <Button type="submit" disabled={create.isPending}>
          Create dashboard
        </Button>
      </DialogFooter>
    </form>
  );
}

/** Renames a dashboard and edits its description, sending the version it was read at. */
export function RenameDashboardDialog({
  org,
  project,
  dashboard,
  open,
  onOpenChange,
  onSaved,
}: {
  org: string;
  project: string;
  dashboard: Pick<Dashboard, "id" | "name" | "description" | "version">;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onSaved: (dashboard: Dashboard) => void;
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <RenameForm
          org={org}
          project={project}
          dashboard={dashboard}
          onCancel={() => onOpenChange(false)}
          onSaved={onSaved}
        />
      </DialogContent>
    </Dialog>
  );
}

function RenameForm({
  org,
  project,
  dashboard,
  onCancel,
  onSaved,
}: {
  org: string;
  project: string;
  dashboard: Pick<Dashboard, "id" | "name" | "description" | "version">;
  onCancel: () => void;
  onSaved: (dashboard: Dashboard) => void;
}) {
  const update = useUpdateDashboard(org, project, dashboard.id);
  const [name, setName] = useState(dashboard.name);
  const [description, setDescription] = useState(dashboard.description ?? "");
  const [submitted, setSubmitted] = useState(false);
  const stale = isStaleDashboard(update.error);
  const failure = failureOf(update.error, stale);
  const issues = clientIssues(name, description, submitted);

  async function submit(): Promise<void> {
    setSubmitted(true);
    if (!issues.valid) {
      return;
    }
    const saved = await update
      .mutateAsync({ version: dashboard.version, name, description })
      .catch(() => undefined);
    if (saved) {
      onSaved(saved);
    }
  }

  return (
    <form
      className="contents"
      onSubmit={(event) => {
        event.preventDefault();
        void submit();
      }}
    >
      <DialogHeader>
        <DialogTitle>Rename dashboard</DialogTitle>
        <DialogDescription>Change the name and description.</DialogDescription>
      </DialogHeader>
      <DashboardFields
        name={name}
        description={description}
        onName={setName}
        onDescription={setDescription}
        failure={failure}
        issues={issues}
      />
      {failure.form ? <p className="text-caption text-destructive">{failure.form}</p> : null}
      <DialogFooter>
        <Button type="button" variant="outline" onClick={onCancel}>
          Cancel
        </Button>
        <Button type="submit" disabled={update.isPending}>
          Save
        </Button>
      </DialogFooter>
    </form>
  );
}

/** Copies a dashboard's definition into a new dashboard with the given name, then opens it. */
export function DuplicateDashboardDialog({
  org,
  project,
  defaultName,
  definition,
  open,
  onOpenChange,
  onCreated,
}: {
  org: string;
  project: string;
  defaultName: string;
  definition: Definition;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onCreated: (dashboard: Dashboard) => void;
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DuplicateForm
          org={org}
          project={project}
          defaultName={defaultName}
          definition={definition}
          onCancel={() => onOpenChange(false)}
          onCreated={onCreated}
        />
      </DialogContent>
    </Dialog>
  );
}

function DuplicateForm({
  org,
  project,
  defaultName,
  definition,
  onCancel,
  onCreated,
}: {
  org: string;
  project: string;
  defaultName: string;
  definition: Definition;
  onCancel: () => void;
  onCreated: (dashboard: Dashboard) => void;
}) {
  const create = useCreateDashboard(org, project);
  const [name, setName] = useState(defaultName);
  const [submitted, setSubmitted] = useState(false);
  const failure = failureOf(create.error, false);
  const issues = clientIssues(name, "", submitted);

  async function submit(): Promise<void> {
    setSubmitted(true);
    if (!issues.valid) {
      return;
    }
    const copy = await create.mutateAsync({ name, definition }).catch(() => undefined);
    if (copy) {
      onCreated(copy);
    }
  }

  return (
    <form
      className="contents"
      onSubmit={(event) => {
        event.preventDefault();
        void submit();
      }}
    >
      <DialogHeader>
        <DialogTitle>Duplicate dashboard</DialogTitle>
        <DialogDescription>Create a copy of its widgets under a new name.</DialogDescription>
      </DialogHeader>
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="dashboard-name">Name</Label>
        <Input
          id="dashboard-name"
          value={name}
          aria-invalid={(issues.name ?? failure.name) !== undefined}
          onChange={(event) => setName(event.target.value)}
        />
        {(issues.name ?? failure.name) ? (
          <p className="text-caption text-destructive">{issues.name ?? failure.name}</p>
        ) : null}
      </div>
      {failure.form ? <p className="text-caption text-destructive">{failure.form}</p> : null}
      <DialogFooter>
        <Button type="button" variant="outline" onClick={onCancel}>
          Cancel
        </Button>
        <Button type="submit" disabled={create.isPending}>
          Duplicate
        </Button>
      </DialogFooter>
    </form>
  );
}

/** Deletes a dashboard after a confirmation, then calls `onDeleted`. */
export function DeleteDashboardDialog({
  org,
  project,
  dashboard,
  open,
  onOpenChange,
  onDeleted,
}: {
  org: string;
  project: string;
  dashboard: Pick<Dashboard, "id" | "name">;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onDeleted: () => void;
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DeleteForm
          org={org}
          project={project}
          dashboard={dashboard}
          onCancel={() => onOpenChange(false)}
          onDeleted={onDeleted}
        />
      </DialogContent>
    </Dialog>
  );
}

function DeleteForm({
  org,
  project,
  dashboard,
  onCancel,
  onDeleted,
}: {
  org: string;
  project: string;
  dashboard: Pick<Dashboard, "id" | "name">;
  onCancel: () => void;
  onDeleted: () => void;
}) {
  const remove = useDeleteDashboard(org, project, dashboard.id);
  return (
    <>
      <DialogHeader>
        <DialogTitle>Delete dashboard</DialogTitle>
        <DialogDescription>Delete {dashboard.name}? This cannot be undone.</DialogDescription>
      </DialogHeader>
      {remove.error ? (
        <p className="text-caption text-destructive">
          {problemMessage(remove.error, "Could not delete the dashboard.")}
        </p>
      ) : null}
      <DialogFooter>
        <Button type="button" variant="outline" onClick={onCancel}>
          Cancel
        </Button>
        <Button
          type="button"
          variant="destructive"
          disabled={remove.isPending}
          onClick={() => {
            void remove.mutateAsync().then(onDeleted, () => undefined);
          }}
        >
          Delete dashboard
        </Button>
      </DialogFooter>
    </>
  );
}

/** Resets the project's Overview to the built-in preset after a confirmation, then calls `onReset`. */
export function ResetOverviewDialog({
  org,
  project,
  open,
  onOpenChange,
  onReset,
}: {
  org: string;
  project: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onReset: () => void;
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <ResetForm
          org={org}
          project={project}
          onCancel={() => onOpenChange(false)}
          onReset={onReset}
        />
      </DialogContent>
    </Dialog>
  );
}

function ResetForm({
  org,
  project,
  onCancel,
  onReset,
}: {
  org: string;
  project: string;
  onCancel: () => void;
  onReset: () => void;
}) {
  const reset = useResetOverview(org, project);
  return (
    <>
      <DialogHeader>
        <DialogTitle>Reset the Overview?</DialogTitle>
        <DialogDescription>
          The project's Overview goes back to the built-in one. Changes made to it are lost.
        </DialogDescription>
      </DialogHeader>
      {reset.error ? (
        <p className="text-caption text-destructive">
          {problemMessage(reset.error, "Could not reset the Overview.")}
        </p>
      ) : null}
      <DialogFooter>
        <Button type="button" variant="outline" onClick={onCancel}>
          Cancel
        </Button>
        <Button
          type="button"
          variant="destructive"
          disabled={reset.isPending}
          onClick={() => {
            void reset.mutateAsync().then(onReset, () => undefined);
          }}
        >
          Reset
        </Button>
      </DialogFooter>
    </>
  );
}
