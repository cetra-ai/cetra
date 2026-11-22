type ExternalNavigator = {
  /** Leaves the app for a URL with a GET navigation (a history entry is added). */
  navigate: (url: string) => void;
  /** Leaves the app for a URL without keeping the current page in history. */
  replace: (url: string) => void;
};

const browserNavigator: ExternalNavigator = {
  navigate: (url) => {
    window.location.assign(url);
  },
  replace: (url) => {
    window.location.replace(url);
  },
};

let current: ExternalNavigator = browserNavigator;

/** Leaves the app for `url` (a full URL outside the router). */
export function navigateExternal(url: string): void {
  current.navigate(url);
}

/** Leaves the app for `url`, replacing the current history entry, so Back does not return to it. */
export function replaceExternal(url: string): void {
  current.replace(url);
}

/** Tests replace both; `undefined` restores the browser's. */
export function setExternalNavigator(next: ExternalNavigator | undefined): void {
  current = next ?? browserNavigator;
}
