// A shot's `storage` is a map of localStorage keys to string values, written after the per-shot
// clear and before the page loads. Throws on anything else, before the server starts.
export function checkStorage(shots) {
  for (const shot of shots) {
    if (shot.storage === undefined) continue;
    const map = shot.storage;
    if (typeof map !== "object" || map === null || Array.isArray(map)) {
      throw new Error(`screenshots.json: shot "${shot.name}": storage must be an object`);
    }
    for (const [key, value] of Object.entries(map)) {
      if (key === "" || typeof value !== "string") {
        throw new Error(
          `screenshots.json: shot "${shot.name}": storage "${key}" must be a non-empty key with a string value`,
        );
      }
    }
  }
}
