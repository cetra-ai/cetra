/**
 * Test helper: a scripted `RestFetch` that records each request and sleep.
 *
 * Not part of the build (no entry imports this file); type-checked by
 * tsconfig.test.json only.
 */

import type { RestFetch } from "./client.js";

export interface RecordedRequest {
  method: string;
  url: string;
  headers: Record<string, string>;
  body: string | undefined;
}

/** A canned answer: a JSON body (sent as `application/json`) or raw text. */
export interface Reply {
  status: number;
  json?: unknown;
  text?: string;
  headers?: Record<string, string>;
}

export class FakeRest {
  readonly requests: RecordedRequest[] = [];
  readonly sleeps: number[] = [];
  readonly fetch: RestFetch;

  /** `answer` gets each request in turn; an `Error` answer is a network failure. */
  constructor(answer: (request: RecordedRequest) => Reply | Error) {
    this.fetch = async (url, init) => {
      const request: RecordedRequest = {
        method: init.method,
        url,
        headers: init.headers,
        body: init.body,
      };
      this.requests.push(request);
      const reply = answer(request);
      if (reply instanceof Error) {
        throw reply;
      }
      const text = reply.json !== undefined ? JSON.stringify(reply.json) : (reply.text ?? "");
      const headers: Record<string, string> = { ...reply.headers };
      if (reply.json !== undefined && headers["Content-Type"] === undefined) {
        headers["Content-Type"] = "application/json";
      }
      return {
        status: reply.status,
        ok: reply.status >= 200 && reply.status < 300,
        headers: {
          get: (name: string) => {
            const key = Object.keys(headers).find(
              (candidate) => candidate.toLowerCase() === name.toLowerCase(),
            );
            return key === undefined ? null : (headers[key] ?? null);
          },
        },
        text: async () => text,
      };
    };
  }

  readonly sleep = async (ms: number): Promise<void> => {
    this.sleeps.push(ms);
  };
}

/** Answers from a fixed list, in order; running out is a test failure. */
export function inOrder(
  ...replies: (Reply | Error)[]
): (request: RecordedRequest) => Reply | Error {
  const queue = [...replies];
  return () => {
    const next = queue.shift();
    if (next === undefined) {
      throw new Error("no scripted reply left");
    }
    return next;
  };
}

export const BASE = "http://llmobs.test/api/v1/projects/current";
