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
 * Whether `userId` may take or renew the item's lease: the item is pending, and it has no live
 * lease or the caller holds it (D-AQ-4). The server omits `leased_by_user_id` and
 * `lease_expires_at` once a lease has expired, so the client does not compare clocks.
 */
export function canLease(item: Schema<"AnnotationQueueItem">, userId: string): boolean {
  if (item.status !== "pending") {
    return false;
  }
  return item.leased_by_user_id === undefined || item.leased_by_user_id === userId;
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
    // Best effort: the lease also ends by itself after five minutes. Keepalive lets the request
    // outlive the page, so `pagehide` (tab closed or navigated away) sends it too.
    const release = () => {
      void api
        .DELETE("/api/v1/projects/{project}/annotation-queues/{id}/items/{item_id}/lease", {
          ...path,
          keepalive: true,
        })
        .catch(() => undefined);
    };
    void claim();
    const timer = window.setInterval(() => {
      void claim();
    }, LEASE_RENEW_MS);
    window.addEventListener("pagehide", release);
    return () => {
      active = false;
      window.clearInterval(timer);
      window.removeEventListener("pagehide", release);
      release();
    };
  }, [enabled, org, project, queueId, itemId]);

  return lease;
}
