import type { Schema } from "@/api/types";
import { Badge } from "@/components/ui/badge";

/** The kind of an alert event: "Fired", "Resolved", "No data" or "Reminder". */
export function EventKindBadge({ kind }: { kind: Schema<"AlertEventKind"> }) {
  switch (kind) {
    case "fired":
      return <Badge variant="destructive">Fired</Badge>;
    case "resolved":
      return <Badge variant="secondary">Resolved</Badge>;
    case "no_data":
      return <Badge variant="outline">No data</Badge>;
    case "reminder":
      return <Badge variant="outline">Reminder</Badge>;
  }
}
