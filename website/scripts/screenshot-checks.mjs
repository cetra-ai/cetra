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

// A shot's `clip` is a CSS selector (one element), or a list of at least two selectors whose elements are
// captured together, as the smallest rectangle around them. Throws on anything else, before the server starts.
export function checkClip(shots) {
  for (const shot of shots) {
    if (shot.clip === undefined) continue;
    const label = `screenshots.json: shot "${shot.name}"`;
    const selectors = Array.isArray(shot.clip) ? shot.clip : [shot.clip];
    if (Array.isArray(shot.clip) && shot.clip.length < 2) {
      throw new Error(`${label}: a clip list needs at least two selectors; use a string for one`);
    }
    for (const selector of selectors) {
      if (typeof selector !== "string" || selector === "") {
        throw new Error(`${label}: clip must be a selector or a list of selectors`);
      }
    }
  }
}

// A shot's `clipRect` is a rectangle in CSS px of its viewport, `{ x, y, width, height }`, with
// x and y at least 0 and width and height positive integers. A shot has `clip` (an element) or
// `clipRect` (a rectangle), not both. Throws on anything else, before the server starts.
export function checkClipRect(shots) {
  for (const shot of shots) {
    if (shot.clipRect === undefined) continue;
    const label = `screenshots.json: shot "${shot.name}"`;
    if (shot.clip !== undefined) {
      throw new Error(`${label}: a shot has clip or clipRect, not both`);
    }
    const rect = shot.clipRect;
    if (typeof rect !== "object" || rect === null || Array.isArray(rect)) {
      throw new Error(`${label}: clipRect must be an object`);
    }
    for (const key of ["x", "y", "width", "height"]) {
      const value = rect[key];
      const min = key === "width" || key === "height" ? 1 : 0;
      if (!Number.isInteger(value) || value < min) {
        throw new Error(
          `${label}: clipRect.${key} must be an integer of at least ${min}, got ${JSON.stringify(value)}`,
        );
      }
    }
  }
}
