import type { Schema } from "@/api/types";

/**
 * The provider the login and invite pages start at once (R7, D-S-5): password sign-in is
 * off, no org provider routes a domain (`sso_discovery`, D-P-60), and exactly one instance
 * provider is configured. `undefined` otherwise.
 */
export function onlyProvider(config: Schema<"AuthConfig">): Schema<"AuthProvider"> | undefined {
  if (config.password_enabled || config.sso_discovery || config.providers.length !== 1) {
    return undefined;
  }
  return config.providers[0];
}
