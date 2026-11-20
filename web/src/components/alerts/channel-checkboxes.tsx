import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { channelsQuery } from "@/api/queries-alerts";
import { Checkbox } from "@/components/ui/checkbox";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { channelKindLabel } from "@/lib/alerts";

const CHANNEL_LIMIT = 10;

/**
 * The project's channels as checkboxes, for the Notifications of a rule or a source. The caller
 * renders the fieldset and its legend. `idPrefix` names the ids: `<prefix>-channel-<id>` and
 * `<prefix>-channels-error`.
 */
export function ChannelCheckboxes(props: {
  org: string;
  project: string;
  idPrefix: string;
  selected: string[];
  onToggle: (id: string, checked: boolean) => void;
  disabled?: boolean;
  limit?: number;
  error?: string;
}) {
  const {
    org,
    project,
    idPrefix,
    selected,
    onToggle,
    disabled = false,
    limit = CHANNEL_LIMIT,
    error,
  } = props;
  const channels = useQuery(channelsQuery(org, project));

  if (channels.error) {
    throw channels.error;
  }
  if (channels.isPending) {
    return <Skeleton className="h-8 w-full" />;
  }
  const channelList = channels.data?.data ?? [];
  if (channelList.length === 0) {
    return (
      <p className="text-body text-muted-foreground">
        No channels yet.{" "}
        <Link
          to="/$org/$project/settings/channels"
          params={{ org, project }}
          className="underline underline-offset-4"
        >
          Add a channel
        </Link>
      </p>
    );
  }
  return (
    <div className="flex flex-col gap-2">
      {channelList.map((channel) => {
        const checked = selected.includes(channel.id);
        const id = `${idPrefix}-channel-${channel.id}`;
        return (
          <div key={channel.id} className="flex items-center gap-2">
            <Checkbox
              id={id}
              checked={checked}
              disabled={disabled || (!checked && selected.length >= limit)}
              onCheckedChange={(next) => {
                onToggle(channel.id, next === true);
              }}
            />
            <Label htmlFor={id}>{`${channel.name} (${channelKindLabel(channel.kind)})`}</Label>
          </div>
        );
      })}
      {error ? (
        <p id={`${idPrefix}-channels-error`} className="text-caption text-destructive">
          {error}
        </p>
      ) : null}
    </div>
  );
}
