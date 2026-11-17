import { IdText } from "@/components/id-text";

/** A user id, "API key" for a key (`key:<id>`), or a dash when the version has no author. */
export function CreatedBy({ createdBy }: { createdBy: string | undefined }) {
  if (createdBy === undefined) {
    return "—";
  }
  if (createdBy.startsWith("key:")) {
    return "API key";
  }
  return <IdText value={createdBy} kind="user" />;
}
