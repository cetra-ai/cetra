// The repository's star count, fetched once at build time (design §3 "Single sources").
// Any failure returns null and the header shows "GitHub" without a number.

/**
 * @param {string} repo `owner/name`
 * @param {{ fetchImpl?: typeof fetch, timeoutMs?: number }} [options]
 * @returns {Promise<number | null>}
 */
export async function fetchStars(repo, { fetchImpl = fetch, timeoutMs = 3000 } = {}) {
  try {
    /** @type {Record<string, string>} */
    const headers = { accept: "application/vnd.github+json", "user-agent": "cetra-website-build" };
    if (process.env.GITHUB_TOKEN) headers.authorization = `Bearer ${process.env.GITHUB_TOKEN}`;
    const res = await fetchImpl(`https://api.github.com/repos/${repo}`, {
      headers,
      signal: AbortSignal.timeout(timeoutMs),
    });
    if (!res.ok) return null;
    const body = await res.json();
    const n = body?.stargazers_count;
    return Number.isInteger(n) && n >= 0 ? n : null;
  } catch {
    return null;
  }
}

/**
 * 999 → "999", 1000 → "1k", 1250 → "1.3k", 12345 → "12.3k", 1000000 → "1M".
 * @param {number} n
 */
export function formatStars(n) {
  if (n < 1000) return String(n);
  if (Math.round(n / 100) < 10_000) return `${trim(n / 1000)}k`;
  return `${trim(n / 1_000_000)}M`;
}

/** @param {number} x */
function trim(x) {
  return (Math.round(x * 10) / 10).toFixed(1).replace(/\.0$/, "");
}

let cached;
/** One fetch per build, shared by every page. */
export function starsOnce(repo) {
  cached ??= fetchStars(repo);
  return cached;
}
