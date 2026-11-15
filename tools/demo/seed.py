#!/usr/bin/env python3
"""Seed a running llmobs server with demo traces, sessions, end users and scores.

It replays the OTLP fixtures in `fixtures/otlp/` over OTLP/HTTP JSON with fresh trace and span
IDs, timestamps spread over the last days, and added session, user, environment and tag
attributes. Then it writes scores through `POST /api/v1/scores`. Standard library only.

    python3 tools/demo/seed.py --key sk-llmobs-... [--url http://localhost:8080] [--traces 300]
"""

from __future__ import annotations

import argparse
import copy
import json
import random
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from collections.abc import Iterator
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "fixtures" / "otlp"
# `edge` cases pin corner cases (rejected spans, odd costs); the rest look like real apps.
SKIP_GROUPS = {"edge"}
NANOS = 1_000_000_000

USERS = [
    "alice@acme.test",
    "bob@acme.test",
    "carol@globex.test",
    "dave@globex.test",
    "erin@initech.test",
    "frank@initech.test",
    "grace@umbrella.test",
    "heidi@umbrella.test",
    "ivan@hooli.test",
    "judy@hooli.test",
    "mallory@stark.test",
    "oscar@wayne.test",
]
SERVICES = ["support-bot", "search-api", "agent-worker", "docs-assistant"]
ENVIRONMENTS = ["production", "production", "production", "staging"]
RELEASES = ["1.4.0", "1.4.1", "1.5.0"]
TAGS = ["beta", "checkout", "onboarding", "vip", "mobile", "web"]
ERRORS = ["upstream timeout", "rate limited by provider", "tool returned invalid JSON"]
TONES = ["friendly", "neutral", "curt"]


def load_fixtures() -> list[tuple[str, dict]]:
    cases = []
    for case_file in sorted(FIXTURES.glob("*/*/case.json")):
        group = case_file.parent.parent.name
        if group in SKIP_GROUPS:
            continue
        case = json.loads(case_file.read_text())
        # Cases with their own ingest context or limits place spans out of range on purpose.
        if "context" in case or "settings" in case:
            continue
        request = json.loads((case_file.parent / "input.json").read_text())
        name = f"{group}/{case_file.parent.name}"
        cases.append((name, request))
    return cases


def spans_of(request: dict) -> Iterator[dict]:
    for resource in request.get("resourceSpans", []):
        for scope in resource.get("scopeSpans", []):
            yield from scope.get("spans", [])


def attr(key: str, value: str | list[str]) -> dict:
    if isinstance(value, list):
        return {
            "key": key,
            "value": {"arrayValue": {"values": [{"stringValue": v} for v in value]}},
        }
    return {"key": key, "value": {"stringValue": value}}


def rand_hex(rng: random.Random, n_bytes: int) -> str:
    return rng.randbytes(n_bytes).hex()


def make_trace(
    rng: random.Random, request: dict, start_ns: int, ctx: dict
) -> tuple[dict, list[str]]:
    """A copy of `request` with new IDs, moved to `start_ns`, carrying `ctx`.

    Returns the request and the new trace IDs.
    """
    request = copy.deepcopy(request)
    spans = list(spans_of(request))
    trace_ids: dict[str, str] = {}
    span_ids: dict[str, str] = {}
    for span in spans:
        trace_ids.setdefault(span["traceId"], rand_hex(rng, 16))
        span_ids.setdefault(span["spanId"], rand_hex(rng, 8))
    starts = [
        int(s["startTimeUnixNano"]) for s in spans if int(s.get("startTimeUnixNano", "0")) > 0
    ]
    shift = start_ns - min(starts) if starts else 0
    error_root = ctx.get("error")
    for span in spans:
        span["traceId"] = trace_ids[span["traceId"]]
        span["spanId"] = span_ids[span["spanId"]]
        parent = span.get("parentSpanId", "")
        if parent:
            # A parent outside the request stays a dangling parent under a fresh id.
            span["parentSpanId"] = span_ids.setdefault(parent, rand_hex(rng, 8))
        for link in span.get("links", []):
            link["traceId"] = trace_ids.get(link["traceId"], rand_hex(rng, 16))
            link["spanId"] = span_ids.get(link["spanId"], rand_hex(rng, 8))
        for key in ("startTimeUnixNano", "endTimeUnixNano"):
            if int(span.get(key, "0")) > 0:
                span[key] = str(int(span[key]) + shift)
        for event in span.get("events", []):
            if int(event.get("timeUnixNano", "0")) > 0:
                event["timeUnixNano"] = str(int(event["timeUnixNano"]) + shift)
        extra = [attr("llmobs.user.id", ctx["user"]), attr("llmobs.tags", ctx["tags"])]
        if ctx.get("session"):
            extra.append(attr("llmobs.session.id", ctx["session"]))
        span["attributes"] = extra + span.get("attributes", [])
        if error_root and not parent:
            span["status"] = {"code": 2, "message": error_root}
    for resource in request.get("resourceSpans", []):
        attrs = [
            a
            for a in resource.setdefault("resource", {}).get("attributes", [])
            if a["key"]
            not in (
                "service.name",
                "deployment.environment",
                "deployment.environment.name",
                "service.version",
            )
        ]
        attrs += [
            attr("service.name", ctx["service"]),
            attr("deployment.environment.name", ctx["environment"]),
            attr("service.version", ctx["release"]),
        ]
        resource["resource"]["attributes"] = attrs
    return request, list(trace_ids.values())


def post(url: str, key: str, body: dict) -> dict:
    data = json.dumps(body).encode()
    req = urllib.request.Request(
        url,
        data=data,
        method="POST",
        headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=60) as response:
            raw = response.read()
    except urllib.error.HTTPError as error:
        sys.exit(f"POST {url} failed: {error.code} {error.read().decode(errors='replace')[:500]}")
    except urllib.error.URLError as error:
        sys.exit(f"POST {url} failed: {error.reason} (is `llmobs serve` running?)")
    return json.loads(raw) if raw else {}


SCORE_DEFINITIONS = [
    {
        "name": "helpfulness",
        "data_type": "numeric",
        "min_value": 0,
        "max_value": 1,
        "description": "How useful the answer was.",
    },
    {
        "name": "quality",
        "data_type": "categorical",
        "categories": [
            {"label": "good", "value": 1},
            {"label": "ok", "value": 0.5},
            {"label": "bad", "value": 0},
        ],
        "description": "Reviewer verdict.",
    },
]


def define_scores(url: str, project: str, email: str, password: str) -> None:
    """Signs in and creates the score definitions that the Annotate tab offers.

    An `sk` key cannot manage score definitions, so this uses a web session. Cookie-authenticated
    writes must carry the server's `Origin` (CSRF).
    """
    opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor())
    headers = {"Content-Type": "application/json", "Origin": url}

    def send(path: str, body: dict) -> int:
        req = urllib.request.Request(
            f"{url}{path}", data=json.dumps(body).encode(), method="POST", headers=headers
        )
        try:
            with opener.open(req, timeout=30) as response:
                return response.status
        except urllib.error.HTTPError as error:
            return error.code

    status = send("/api/auth/login", {"email": email, "password": password})
    if status != 200:
        sys.exit(f"sign-in as {email} failed: HTTP {status}")
    path = f"/api/v1/projects/{urllib.parse.quote(project, safe='')}/score-definitions"
    for definition in SCORE_DEFINITIONS:
        status = send(path, definition)
        # 409: it exists already, from an earlier run.
        if status not in (200, 201, 409):
            sys.exit(f"creating score definition {definition['name']} failed: HTTP {status}")
    print(f"score definitions ready: {', '.join(d['name'] for d in SCORE_DEFINITIONS)}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--url", default="http://localhost:8080", help="llmobs base URL")
    parser.add_argument("--key", required=True, help="project secret key (sk-llmobs-...)")
    parser.add_argument("--traces", type=int, default=300, help="number of traces to send")
    parser.add_argument(
        "--days", type=float, default=7, help="spread traces over this many past days"
    )
    parser.add_argument("--seed", type=int, default=None, help="random seed (default: random)")
    parser.add_argument(
        "--project", default="demo/demo", help="org-slug/project-slug, for score definitions"
    )
    parser.add_argument(
        "--email", help="admin email; with --password, also create score definitions"
    )
    parser.add_argument("--password", help="admin password")
    args = parser.parse_args()

    if args.email and args.password:
        define_scores(args.url, args.project, args.email, args.password)

    rng = random.Random(args.seed)
    fixtures = load_fixtures()
    if not fixtures:
        sys.exit(f"no fixtures found under {FIXTURES}")
    now = time.time_ns()
    window = max(args.days * 86400, 60)
    span_count = 0
    rejected = 0
    traces: list[dict] = []
    sessions: dict[str, str] = {}
    batch: list[dict] = []

    def flush() -> None:
        nonlocal rejected
        if not batch:
            return
        body = {"resourceSpans": [rs for request in batch for rs in request["resourceSpans"]]}
        answer = post(f"{args.url}/v1/traces", args.key, body)
        rejected += int(answer.get("partialSuccess", {}).get("rejectedSpans", 0) or 0)
        batch.clear()

    for i in range(args.traces):
        _, request = rng.choice(fixtures)
        # A fifth of the traffic lands in the last hour and a third in the last day, so the
        # default 24-hour views and the dashboard are full; the rest spreads over the window.
        roll = rng.random()
        horizon = 3600 if roll < 0.2 else 86400 if roll < 0.5 else window
        age = rng.uniform(1, min(horizon, window))
        user = rng.choice(USERS)
        session = None
        if rng.random() < 0.6:
            # A few sessions per user, each with several traces.
            session = f"sess-{USERS.index(user):02d}-{rng.randrange(4)}"
            sessions[session] = user
        ctx = {
            "user": user,
            "session": session,
            "service": rng.choice(SERVICES),
            "environment": rng.choice(ENVIRONMENTS),
            "release": rng.choice(RELEASES),
            "tags": sorted(rng.sample(TAGS, rng.randrange(3))),
            "error": rng.choice(ERRORS) if rng.random() < 0.07 else None,
        }
        trace, trace_ids = make_trace(rng, request, now - int(age * NANOS), ctx)
        span_count += sum(1 for _ in spans_of(trace))
        batch.append(trace)
        for trace_id in trace_ids:
            traces.append({"trace_id": trace_id, "user": user, "session": session})
        if len(batch) >= 20:
            flush()
        if (i + 1) % 100 == 0:
            print(f"sent {i + 1} traces")
    flush()
    print(
        f"sent {args.traces} traces ({span_count} spans, {rejected} rejected)"
        f" from {len(fixtures)} fixtures"
    )

    scores = []
    for t in traces:
        common = {"target": {"type": "trace", "trace_id": t["trace_id"]}, "user_id": t["user"]}
        if t["session"]:
            common["session_id"] = t["session"]
        if rng.random() < 0.5:
            scores.append(
                {
                    **common,
                    "name": "helpfulness",
                    "value": {"data_type": "numeric", "value": round(rng.betavariate(5, 2), 2)},
                }
            )
        if rng.random() < 0.3:
            scores.append(
                {
                    **common,
                    "name": "thumbs_up",
                    "value": {"data_type": "boolean", "value": rng.random() < 0.8},
                }
            )
        if rng.random() < 0.2:
            scores.append(
                {
                    **common,
                    "name": "tone",
                    "reason": "seeded by tools/demo/seed.py",
                    "value": {"data_type": "categorical", "value": {"label": rng.choice(TONES)}},
                }
            )
    for session, user in sessions.items():
        scores.append(
            {
                "target": {"type": "session", "session_id": session},
                "user_id": user,
                "name": "resolved",
                "value": {"data_type": "boolean", "value": rng.random() < 0.7},
            }
        )
    for start in range(0, len(scores), 1000):
        post(f"{args.url}/api/v1/scores", args.key, {"scores": scores[start : start + 1000]})
    print(f"wrote {len(scores)} scores on {len(traces)} traces and {len(sessions)} sessions")


if __name__ == "__main__":
    main()
