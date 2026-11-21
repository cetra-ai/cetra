import { useQueries, useQueryClient } from "@tanstack/react-query";
import { createFileRoute, Link, useNavigate, useRouter } from "@tanstack/react-router";
import { Loader2 } from "lucide-react";
import { type FormEvent, useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage, problemOf } from "@/api/problem";
import type { Schema } from "@/api/types";
import { AuthLayout } from "@/components/auth/auth-layout";
import { AUTH_LINK_CLASS } from "@/components/auth/link-class";
import { FormError } from "@/components/form/form-error";
import { SubmitButton } from "@/components/form/submit-button";
import { TextField } from "@/components/form/text-field";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { PRODUCT_NAME } from "@/product";

export const Route = createFileRoute("/device")({
  component: DevicePage,
});

type Org = Schema<"OrgMembership">;
type Project = Schema<"Project">;
type Choice = { org: Org; project: Project };

const TITLE = `Connect the ${PRODUCT_NAME} CLI`;
const UNKNOWN_CODE =
  "This code is unknown or has expired. Run the command again to get a new code.";
const NO_PROJECT =
  "You need the admin role on a project to connect the CLI. Ask an admin of your organization.";
const LOGIN_TEXT =
  "This creates a personal access token that can read and write, valid for 90 days.";
const INIT_TEXT =
  "This creates a read-only personal access token, valid for 90 days, and a key that can only send traces to the project you choose.";

/** The projects of one org, for the picker. 1000 is the server's largest page. */
function projectsOptions(org: string) {
  return $api.queryOptions("get", "/api/v1/orgs/{org}/projects", {
    params: { path: { org }, query: { limit: 1000 } },
  });
}

/** Admins and owners may manage a project's keys and create projects (spec 08 § Access). */
function canManage(role: string | undefined): boolean {
  return role === "admin" || role === "owner";
}

function DevicePage() {
  const router = useRouter();
  // The raw query string: the router JSON-parses search values, so `code=1234` would arrive
  // as a number and `project` would lose its form.
  const params = new URLSearchParams(router.history.location.search);
  const code = params.get("code") || undefined;
  const project = params.get("project") || undefined;
  const { data: me } = $api.useSuspenseQuery("get", "/api/auth/me");
  if (code === undefined) {
    return <CodeEntry project={project} />;
  }
  return (
    <DeviceApproval
      key={code}
      code={code}
      project={project}
      orgs={me.orgs.filter((org) => !org.demo)}
    />
  );
}

/** `/device` without a code: the user types the code the terminal shows. */
function CodeEntry({ project }: { project: string | undefined }) {
  const navigate = useNavigate();
  const [value, setValue] = useState("");
  const [error, setError] = useState<string | undefined>();

  function onSubmit(event: FormEvent) {
    event.preventDefault();
    const trimmed = value.trim();
    if (trimmed.length === 0) {
      setError("Enter the code that the terminal shows.");
      return;
    }
    setError(undefined);
    const query = new URLSearchParams({ code: trimmed });
    if (project !== undefined) {
      query.set("project", project);
    }
    void navigate({ href: `/device?${query.toString()}` });
  }

  return (
    <AuthLayout title={TITLE} description="Enter the code that the terminal shows.">
      <form noValidate onSubmit={onSubmit} className="flex flex-col gap-4">
        <TextField
          id="user_code"
          label="Code"
          value={value}
          onChange={setValue}
          placeholder="XXXX-XXXX"
          autoComplete="off"
          error={error}
        />
        <SubmitButton pending={false}>Continue</SubmitButton>
      </form>
    </AuthLayout>
  );
}

/** The approval of one code: shows what it is for and approves or denies it. */
function DeviceApproval({
  code,
  project,
  orgs,
}: {
  code: string;
  project: string | undefined;
  orgs: Org[];
}) {
  const request = $api.useQuery(
    "get",
    "/api/auth/device/{user_code}",
    { params: { path: { user_code: code } } },
    { retry: false },
  );
  const details = request.data;
  const isInit = details?.purpose === "init";
  const approve = $api.useMutation("post", "/api/auth/device/{user_code}/approve", {
    gcTime: 0,
  });
  const deny = $api.useMutation("post", "/api/auth/device/{user_code}/deny", { gcTime: 0 });
  const [picked, setPicked] = useState<string | undefined>();
  // Only an `init` code needs the project lists, so they load once the purpose is known.
  const projectQueries = useQueries({
    queries: orgs.map((org) => ({ ...projectsOptions(org.slug), enabled: isInit })),
  });

  const choices: Choice[] = orgs.flatMap((org, index) =>
    (projectQueries[index]?.data?.data ?? []).map((item) => ({ org, project: item })),
  );
  const eligible = choices.filter((choice) => canManage(choice.project.role));
  const preferred = eligible.find(
    (choice) => `${choice.org.slug}/${choice.project.slug}` === project,
  );
  const selected =
    eligible.find((choice) => choice.project.id === picked) ??
    preferred ??
    (eligible.length === 1 ? eligible[0] : undefined);
  const creatable = orgs.filter((org) => canManage(org.role));
  const listsLoaded = projectQueries.every((query) => query.isSuccess || query.isError);
  const listError = projectQueries.find((query) => query.isError)?.error;

  const pending = approve.isPending || deny.isPending;
  const canApprove = !pending && (!isInit || selected !== undefined);

  function approveCode() {
    if (isInit) {
      if (selected === undefined) {
        return;
      }
      approve.mutate({
        params: { path: { user_code: code } },
        body: { project_id: selected.project.id },
      });
      return;
    }
    approve.mutate({ params: { path: { user_code: code } }, body: {} });
  }

  if (approve.isSuccess) {
    return (
      <AuthLayout title={TITLE}>
        <p className="text-body">Done. Return to the terminal.</p>
      </AuthLayout>
    );
  }
  if (deny.isSuccess) {
    return (
      <AuthLayout title={TITLE}>
        <p className="text-body">Cancelled. You can close this window.</p>
      </AuthLayout>
    );
  }
  const unknown = [request.error, approve.error, deny.error].some(
    (error) => problemOf(error)?.status === 404,
  );
  if (unknown) {
    return (
      <AuthLayout title={TITLE}>
        <p className="text-body">{UNKNOWN_CODE}</p>
        <Link to="/device" className={AUTH_LINK_CLASS}>
          Enter another code
        </Link>
      </AuthLayout>
    );
  }
  if (details === undefined) {
    return (
      <AuthLayout title={TITLE}>
        <FormError
          message={
            request.isError ? problemMessage(request.error, "Could not load the code.") : undefined
          }
        />
      </AuthLayout>
    );
  }

  const failure = approve.error
    ? problemMessage(approve.error, "Could not approve the code.")
    : deny.error
      ? problemMessage(deny.error, "Could not deny the code.")
      : undefined;

  return (
    <AuthLayout
      title={TITLE}
      description={
        <>
          Code <code>{code}</code>
        </>
      }
    >
      <p role="note" className="rounded-md border p-3 text-body font-medium">
        Only continue if you just ran <code>cetra-cli init</code>, <code>cetra-cli login</code> or{" "}
        <code>cetra login</code> on this computer. Approving gives that command access to your
        account.
      </p>
      <p className="text-body">
        Computer: {details.host ? <code>{details.host}</code> : "not reported"}
      </p>
      <p className="text-body">{isInit ? INIT_TEXT : LOGIN_TEXT}</p>
      {isInit ? (
        <ProjectPicker
          loading={!listsLoaded}
          listError={listError}
          eligible={eligible}
          orgs={orgs}
          creatable={creatable}
          selectedId={selected?.project.id}
          disabled={pending}
          onPick={setPicked}
        />
      ) : null}
      <FormError message={failure} />
      <div className="flex gap-2">
        <Button type="button" disabled={!canApprove} onClick={approveCode}>
          {approve.isPending ? <Loader2 className="animate-spin" aria-hidden="true" /> : null}
          Approve
        </Button>
        <Button
          type="button"
          variant="outline"
          disabled={pending}
          onClick={() => {
            deny.mutate({ params: { path: { user_code: code } } });
          }}
        >
          Deny
        </Button>
      </div>
    </AuthLayout>
  );
}

/** The project list of an `init` code: pick a project the user may manage, or create one. */
function ProjectPicker({
  loading,
  listError,
  eligible,
  orgs,
  creatable,
  selectedId,
  disabled,
  onPick,
}: {
  loading: boolean;
  listError: unknown;
  eligible: Choice[];
  orgs: Org[];
  creatable: Org[];
  selectedId: string | undefined;
  disabled: boolean;
  onPick: (projectId: string) => void;
}) {
  const [creating, setCreating] = useState(false);
  const listErrorMessage =
    listError === undefined ? undefined : problemMessage(listError, "Could not load the projects.");

  if (loading) {
    return <p className="text-body text-muted-foreground">Loading projects…</p>;
  }
  if (eligible.length === 0 && creatable.length === 0) {
    return (
      <>
        <FormError message={listErrorMessage} />
        <p className="text-body">{NO_PROJECT}</p>
      </>
    );
  }
  return (
    <div className="flex flex-col gap-4">
      <FormError message={listErrorMessage} />
      {eligible.length > 0 ? (
        <div className="flex flex-col gap-2">
          <span id="device-project-label" className="text-body font-medium">
            Project
          </span>
          <RadioGroup
            aria-labelledby="device-project-label"
            value={selectedId ?? ""}
            onValueChange={onPick}
            disabled={disabled}
          >
            {orgs.map((org) => {
              const items = eligible.filter((choice) => choice.org.id === org.id);
              if (items.length === 0) {
                return null;
              }
              return (
                <div key={org.id} className="flex flex-col gap-2">
                  <span className="text-caption text-muted-foreground">{org.name}</span>
                  {items.map((choice) => (
                    <div key={choice.project.id} className="flex items-center gap-2">
                      <RadioGroupItem
                        value={choice.project.id}
                        id={`device-project-${choice.project.id}`}
                      />
                      <Label htmlFor={`device-project-${choice.project.id}`}>
                        {`${org.name} / ${choice.project.name}`}
                      </Label>
                    </div>
                  ))}
                </div>
              );
            })}
          </RadioGroup>
        </div>
      ) : null}
      {creatable.length > 0 ? (
        creating ? (
          <NewProjectForm
            creatable={creatable}
            onCancel={() => {
              setCreating(false);
            }}
            onCreated={(projectId) => {
              setCreating(false);
              onPick(projectId);
            }}
          />
        ) : (
          <Button
            type="button"
            variant="outline"
            className="w-fit"
            disabled={disabled}
            onClick={() => {
              setCreating(true);
            }}
          >
            New project
          </Button>
        )
      ) : null}
    </div>
  );
}

/** The message for a failed project creation, by the status the server answers with. */
function createProjectMessage(error: unknown): string {
  if (problemOf(error)?.code === "plan_limit") {
    return problemMessage(error, "Your plan does not allow another project.");
  }
  if (problemOf(error)?.status === 409) {
    return "A project with this name already exists. Choose another name.";
  }
  if (problemOf(error)?.status === 403) {
    return "Only admins and owners can create projects.";
  }
  return problemMessage(error, "Could not create the project.");
}

/** Creates a project in one of the orgs the user may create in, then reports its id. */
function NewProjectForm({
  creatable,
  onCancel,
  onCreated,
}: {
  creatable: Org[];
  onCancel: () => void;
  onCreated: (projectId: string) => void;
}) {
  const queryClient = useQueryClient();
  const create = $api.useMutation("post", "/api/v1/orgs/{org}/projects");
  const [chosenOrg, setChosenOrg] = useState<string | undefined>();
  const [name, setName] = useState("");
  const [nameError, setNameError] = useState<string | undefined>();
  const orgSlug = chosenOrg ?? creatable[0]?.slug ?? "";
  const fields = fieldErrors(create.error);
  const fieldError = nameError ?? fields.name;
  const message =
    create.isError && fields.name === undefined ? createProjectMessage(create.error) : undefined;

  function onSubmit(event: FormEvent) {
    event.preventDefault();
    const trimmed = name.trim();
    setNameError(trimmed.length === 0 ? "Enter a name." : undefined);
    if (trimmed.length === 0) {
      return;
    }
    create.mutate(
      { params: { path: { org: orgSlug } }, body: { name: trimmed } },
      {
        onSuccess: async (created) => {
          // The picker reads the org's projects from this query, so it must reload first.
          await queryClient.invalidateQueries({ queryKey: projectsOptions(orgSlug).queryKey });
          onCreated(created.id);
        },
      },
    );
  }

  return (
    <form noValidate className="flex flex-col gap-4" onSubmit={onSubmit}>
      <div className="flex flex-col gap-2">
        <Label htmlFor="device-new-org">Organization</Label>
        <Select value={orgSlug} onValueChange={setChosenOrg} disabled={create.isPending}>
          <SelectTrigger id="device-new-org" className="w-full">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {creatable.map((org) => (
              <SelectItem key={org.id} value={org.slug}>
                {org.name}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>
      <TextField
        id="device-new-project-name"
        label="Project name"
        value={name}
        onChange={setName}
        required
        maxLength={100}
        error={fieldError}
        disabled={create.isPending}
      />
      <FormError message={message} />
      <div className="flex gap-2">
        <SubmitButton pending={create.isPending}>Create project</SubmitButton>
        <Button type="button" variant="outline" disabled={create.isPending} onClick={onCancel}>
          Cancel
        </Button>
      </div>
    </form>
  );
}
