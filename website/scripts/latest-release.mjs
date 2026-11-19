// The newest released version in CHANGELOG.md (D-W-9): the first `## [x.y.z]` heading that is
// not `[Unreleased]`, or null before the first release.
export function latestRelease(markdown) {
  const match = markdown.match(/^## \[(\d+\.\d+\.\d+)\]/m);
  return match ? match[1] : null;
}
