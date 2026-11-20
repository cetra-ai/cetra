import { useEffect, useState } from "react";
import { api } from "@/api/client";
import { problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import type { Schema } from "@/api/types";
import { LEASE_RENEW_MS } from "@/lib/annotation-queues";

const LEASE = "/api/v1/projects/{project}/annotation-queues/{id}/items/{item_id}/lease";

export type LeaseState = {
  state: "idle" | "held" | "lost";
  /** The reviewer who holds the item, when the lease was lost to them. */
  holder?: string;
};

/**
 * Whether `userId` may take or renew the item's lease: the item is pending, and its lease is free,
 * expired or already theirs (D-AQ-4). The server sets `leased_by_user_id` and `lease_expires_at`
 * only while a lease is live.
 */
export function canLease(
  item: Schema<"AnnotationQueueItem">,
  userId: string,
  now: number = Date.now(),
): boolean {
  if (item.status !== "pending") {
    return false;
  }
  if (item.leased_by_user_id === undefined || item.leased_by_user_id === userId) {
    return true;
  }
  return item.lease_expires_at !== undefined && Date.parse(item.lease_expires_at) <= now;
}

/**
 * Holds the item's lease while `enabled`: it claims on mount, renews every `LEASE_RENEW_MS`, and
 * releases on leaving or when the item changes. A `409` means another reviewer holds it.
 */
export function useLease(
  org: string,
  project: string,
  queueId: string,
  item: Schema<"AnnotationQueueItem"> | undefined,
  enabled: boolean,
): LeaseState {
  const [lease, setLease] = useState<LeaseState>({ state: "idle" });
  const itemId = item?.id;

  useEffect(() => {
    setLease({ state: "idle" });
    if (!enabled || itemId === undefined) {
      return;
    }
    const path = {
      params: { path: { project: projectRef(org, project), id: queueId, item_id: itemId } },
    };
    let active = true;
    const claim = async () => {
      const { response, error } = await api.POST(LEASE, path);
      if (!active) {
        return;
      }
      if (response.ok) {
        setLease({ state: "held" });
      } else if (response.status === 409) {
        setLease({ state: "lost", holder: problemOf(error)?.errors?.[0]?.message });
      }
    };
    void claim();
    const timer = window.setInterval(() => {
      void claim();
    }, LEASE_RENEW_MS);
    return () => {
      active = false;
      window.clearInterval(timer);
      // Best effort: the lease also ends by itself after five minutes.
      void api
        .DELETE("/api/v1/projects/{project}/annotation-queues/{id}/items/{item_id}/lease", {
          ...path,
          keepalive: true,
        })
        .catch(() => undefined);
    };
  }, [enabled, org, project, queueId, itemId]);

  return lease;
}
