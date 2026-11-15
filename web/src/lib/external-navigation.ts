type ExternalNavigator = {
  /** Leaves the app for a URL with a GET navigation (a history entry is added). */
  navigate: (url: string) => void;
  /** Leaves the app with a top-level form POST; the fields are the body, not the URL. */
  submit: (url: string, fields: Record<string, string>) => void;
};

const browserNavigator: ExternalNavigator = {
  navigate: (url) => {
    window.location.assign(url);
  },
  submit: (url, fields) => {
    const form = document.createElement("form");
    form.method = "post";
    form.action = url;
    for (const [name, value] of Object.entries(fields)) {
      const input = document.createElement("input");
      input.type = "hidden";
      input.name = name;
      input.value = value;
      form.append(input);
    }
    document.body.append(form);
    form.submit();
    // The submission has already read the fields; drop the secret from the DOM.
    form.remove();
  },
};

let current: ExternalNavigator = browserNavigator;

/** Leaves the app for `url` (a full URL outside the router). */
export function navigateExternal(url: string): void {
  current.navigate(url);
}

/** Leaves the app by POSTing `fields` to `url` (a full URL outside the router). */
export function submitExternalForm(url: string, fields: Record<string, string>): void {
  current.submit(url, fields);
}

/** Tests replace both; `undefined` restores the browser's. */
export function setExternalNavigator(next: ExternalNavigator | undefined): void {
  current = next ?? browserNavigator;
}
