import { inspect } from "node:util";
import type { ReadableSpan } from "@opentelemetry/sdk-trace";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { type CetraOptions, type Config, ConfigError, resolveConfig } from "./config.js";
import { resetWarningsForTests } from "./log.js";

const KEY = "sk-cetra-test-key";
const STORED_KEY = "sk-cetra-super-secret-key-42";

type Env = Record<string, string | undefined>;

function resolve(options: Record<string, unknown> = {}, env: Env = {}): Config {
  return resolveConfig(options as CetraOptions, env);
}

function message(options: Record<string, unknown> = {}, env: Env = {}): string {
  try {
    resolve(options, env);
  } catch (error) {
    expect(error).toBeInstanceOf(ConfigError);
    return error instanceof Error ? error.message : String(error);
  }
  throw new Error("expected a ConfigError");
}

function suppressWarn() {
  return vi.spyOn(console, "warn").mockImplementation(() => {});
}

beforeEach(() => {
  resetWarningsForTests();
});

describe("defaults", () => {
  it("fills the spec defaults when only the key is set", () => {
    const config = resolve({ apiKey: KEY });
    expect(config).toMatchObject({
      enabled: true,
      baseUrl: "http://localhost:8080",
      serviceName: undefined,
      environment: undefined,
      release: undefined,
      captureContent: true,
      sampleRate: 1,
      mask: undefined,
      useGlobalProvider: false,
      exportFilter: undefined,
      instrument: [],
    });
    expect(config.apiKey).toBe(KEY);
  });

  it("disables without reading the other options", () => {
    const config = resolve(
      { enabled: false, apiKey: 1, sampleRate: 4 },
      { CETRA_SAMPLE_RATE: "nope", GIT_COMMIT: "abc123", SOURCE_VERSION: "src" },
    );
    expect(config.enabled).toBe(false);
    expect(config.apiKey).toBeUndefined();
    expect(config.baseUrl).toBe("http://localhost:8080");
    expect(config.release).toBeUndefined();
    expect(config.sampleRate).toBe(1);
  });

  it("warns once and runs disabled when the key is missing (D-F-3)", () => {
    const warn = suppressWarn();
    try {
      const config = resolve();
      expect(config.enabled).toBe(false);
      expect(config.apiKey).toBeUndefined();
      expect(config.baseUrl).toBe("http://localhost:8080");
      expect(config.serviceName).toBeUndefined();
      expect(warn).toHaveBeenCalledTimes(1);
      expect(warn).toHaveBeenCalledWith(
        "cetra: no API key; set CETRA_API_KEY, or CETRA_ENABLED=false to silence this; tracing is disabled",
      );
      resolve();
      expect(warn).toHaveBeenCalledTimes(1);
    } finally {
      warn.mockRestore();
    }
  });

  it("validates nothing else when the key is missing", () => {
    const warn = suppressWarn();
    try {
      expect(resolve({ sampleRate: 4 }).enabled).toBe(false);
      expect(resolve({}, { CETRA_BASE_URL: "ftp://bad" }).enabled).toBe(false);
      // Two resolutions, one warning: warnOnce deduplicates by key.
      expect(warn).toHaveBeenCalledTimes(1);
    } finally {
      warn.mockRestore();
    }
  });

  it("still raises on a key with the wrong prefix", () => {
    expect(message({ apiKey: "pk-cetra-nope" })).toBe("apiKey: must start with sk-cetra-");
    expect(message({}, { CETRA_API_KEY: "pk-cetra-nope" })).toBe(
      "CETRA_API_KEY: must start with sk-cetra-",
    );
  });

  it("disables from the env when no key is set", () => {
    const config = resolve({}, { CETRA_ENABLED: "false" });
    expect(config.enabled).toBe(false);
    expect(config.apiKey).toBeUndefined();
    expect(config.baseUrl).toBe("http://localhost:8080");
  });
});

describe("precedence", () => {
  it.each([
    ["apiKey", "CETRA_API_KEY", KEY, "sk-cetra-from-env"],
    ["baseUrl", "CETRA_BASE_URL", "https://arg.example", "https://env.example"],
    ["environment", "CETRA_ENVIRONMENT", "prod", "staging"],
    ["release", "CETRA_RELEASE", "1.2.3", "9.9.9"],
    ["captureContent", "CETRA_CAPTURE_CONTENT", false, "true"],
    ["sampleRate", "CETRA_SAMPLE_RATE", 0.25, "0.5"],
    ["enabled", "CETRA_ENABLED", false, "true"],
  ])("%s prefers the argument over the env var", (option, envName, argument, envValue) => {
    const env: Record<string, string> = { [envName]: String(envValue) };
    if (option !== "apiKey") {
      env.CETRA_API_KEY = KEY;
    }
    const config = resolve({ apiKey: KEY, [option]: argument }, env);
    const value = option === "apiKey" ? config.apiKey : config[option as keyof Config];
    expect(value).toBe(argument);
  });

  it.each([
    ["baseUrl", "CETRA_BASE_URL", "http://env.example", "http://localhost:8080"],
    ["environment", "CETRA_ENVIRONMENT", "staging", undefined],
    ["release", "CETRA_RELEASE", "env", undefined],
    ["captureContent", "CETRA_CAPTURE_CONTENT", false, true],
    ["sampleRate", "CETRA_SAMPLE_RATE", 0.5, 1],
    ["enabled", "CETRA_ENABLED", false, true],
  ])("%s prefers the env var over the default", (option, envName, expected, fallback) => {
    const config = resolve({ apiKey: KEY }, { [envName]: String(expected) });
    expect(config[option as keyof Config]).toBe(expected);
    expect(config[option as keyof Config]).not.toBe(fallback);
  });

  it("treats a blank env value as unset", () => {
    for (const blank of ["", "   ", "\n\t"]) {
      const config = resolve(
        { apiKey: KEY },
        {
          CETRA_BASE_URL: blank,
          CETRA_ENVIRONMENT: blank,
          CETRA_RELEASE: blank,
          CETRA_SAMPLE_RATE: blank,
        },
      );
      expect(config.baseUrl).toBe("http://localhost:8080");
      expect(config.environment).toBeUndefined();
      expect(config.release).toBeUndefined();
      expect(config.sampleRate).toBe(1);
    }
  });

  it("does not trim an explicit argument", () => {
    expect(message({ apiKey: `  ${KEY}` })).toBe("apiKey: must start with sk-cetra-");
  });
});

describe("enabled", () => {
  it("lets an explicit false win over CETRA_ENABLED=true", () => {
    const config = resolve({ enabled: false }, { CETRA_ENABLED: "true" });
    expect(config.enabled).toBe(false);
  });

  it("stops at an invalid enabled before reading the key", () => {
    expect(message({ enabled: "maybe", apiKey: 1 })).toBe("enabled: must be a boolean");
  });

  it.each(["true", "false", "1", "0", "yes", "no", "on", "off", "TRUE", "Off"])(
    "accepts the env spelling %s",
    (spelling) => {
      const config = resolve({ apiKey: KEY }, { CETRA_CAPTURE_CONTENT: spelling });
      expect(config.captureContent).toBe(
        ["true", "1", "yes", "on"].includes(spelling.toLowerCase()),
      );
    },
  );

  it("names the env var when the spelling is unknown", () => {
    expect(message({ apiKey: KEY }, { CETRA_CAPTURE_CONTENT: "maybe" })).toBe(
      "CETRA_CAPTURE_CONTENT: must be a boolean",
    );
  });
});

describe("secrecy", () => {
  it("keeps the key off enumerable surfaces", () => {
    const config = resolve({ apiKey: STORED_KEY });
    expect(JSON.stringify(config)).not.toContain(STORED_KEY);
    expect(inspect(config)).not.toContain(STORED_KEY);
    expect(JSON.stringify({ ...config })).not.toContain(STORED_KEY);
    expect(Object.keys(config)).not.toContain("apiKey");
    expect(config.apiKey).toBe(STORED_KEY);
  });

  it("never echoes a bad key or URL", () => {
    const badKey = "sk-other-super-secret";
    const badUrl = "ftp://user:secret-token@evil.example/path?q=1#frag";
    expect(message({ apiKey: badKey })).toBe("apiKey: must start with sk-cetra-");
    expect(message({ apiKey: badKey })).not.toContain(badKey);
    expect(message({}, { CETRA_API_KEY: badKey })).toBe("CETRA_API_KEY: must start with sk-cetra-");
    expect(message({}, { CETRA_API_KEY: badKey })).not.toContain(badKey);
    expect(message({ apiKey: KEY, baseUrl: badUrl })).not.toContain("evil.example");
    expect(message({ apiKey: KEY, baseUrl: badUrl })).not.toContain("secret-token");
  });
});

describe("baseUrl", () => {
  it("strips trailing slashes", () => {
    const config = resolve({ apiKey: KEY, baseUrl: "https://host/prefix///" });
    expect(config.baseUrl).toBe("https://host/prefix");
  });

  it.each([
    ["http://[::1]:8080", "http://[::1]:8080"],
    ["HTTPS://X.example/", "HTTPS://X.example"],
  ])("accepts %s", (baseUrl, expected) => {
    expect(resolve({ apiKey: KEY, baseUrl }).baseUrl).toBe(expected);
  });

  it.each(["http://[::1", "http://::1]", "http://[zz]", "http://a]b", "http://", "http://:8080"])(
    "rejects the host %s",
    (baseUrl) => {
      expect(message({ apiKey: KEY, baseUrl })).toBe(
        "baseUrl: must be an absolute http or https URL",
      );
    },
  );

  it("allows an empty query or fragment", () => {
    expect(resolve({ apiKey: KEY, baseUrl: "http://host/?" }).baseUrl).toBe("http://host/?");
    expect(resolve({ apiKey: KEY, baseUrl: "http://host/#" }).baseUrl).toBe("http://host/#");
  });

  it.each(["ftp://example.com", "localhost:8080", "http://host/?q=1", "http://host/#f"])(
    "rejects %s",
    (baseUrl) => {
      const text = message({ apiKey: KEY, baseUrl });
      expect(text.startsWith("baseUrl:")).toBe(true);
      expect(text).not.toContain(baseUrl);
    },
  );
});

describe("sampleRate from the env", () => {
  it.each(["1", "0", "0.25", ".5", "1.", "0.0", " 0.5 "])("accepts %j", (raw) => {
    expect(resolve({ apiKey: KEY }, { CETRA_SAMPLE_RATE: raw }).sampleRate).toBe(Number(raw));
  });

  it.each(["+0.5", "-0", "5e-1", "0x1", "0b1", "0o1", "0.2_5", "١", "Infinity", "NaN", "1.5"])(
    "rejects %j",
    (raw) => {
      expect(message({ apiKey: KEY }, { CETRA_SAMPLE_RATE: raw })).toBe(
        "CETRA_SAMPLE_RATE: must be a number between 0 and 1",
      );
    },
  );
});

describe("sampleRate", () => {
  it.each([0, 1, 0.25])("accepts %s", (sampleRate) => {
    expect(resolve({ apiKey: KEY, sampleRate }).sampleRate).toBe(sampleRate);
  });

  it.each([-0.1, 1.5, Number.NaN, Number.POSITIVE_INFINITY, true, "0.5"])(
    "rejects %s",
    (sampleRate) => {
      expect(message({ apiKey: KEY, sampleRate })).toBe(
        "sampleRate: must be a number between 0 and 1",
      );
    },
  );

  it("rejects a non-numeric env value", () => {
    expect(message({ apiKey: KEY }, { CETRA_SAMPLE_RATE: "abc" })).toBe(
      "CETRA_SAMPLE_RATE: must be a number between 0 and 1",
    );
  });
});

describe("environment and release", () => {
  it("rejects the reserved environment without echoing it", () => {
    const text = message({ apiKey: KEY, environment: "cetra-eval" });
    expect(text).toBe("environment: is reserved");
    expect(message({ apiKey: KEY }, { CETRA_ENVIRONMENT: "cetra-eval" })).toBe(
      "CETRA_ENVIRONMENT: is reserved",
    );
  });

  it("rejects the playground environment without echoing it", () => {
    const text = message({ apiKey: KEY, environment: "cetra-playground" });
    expect(text).toBe("environment: is reserved");
    expect(message({ apiKey: KEY }, { CETRA_ENVIRONMENT: "cetra-playground" })).toBe(
      "CETRA_ENVIRONMENT: is reserved",
    );
  });

  it("rejects the assistant environment without echoing it", () => {
    const text = message({ apiKey: KEY, environment: "cetra-assistant" });
    expect(text).toBe("environment: is reserved");
    expect(message({ apiKey: KEY }, { CETRA_ENVIRONMENT: "cetra-assistant" })).toBe(
      "CETRA_ENVIRONMENT: is reserved",
    );
  });

  it("uses the release argument, then CETRA_RELEASE, and ignores git env vars", () => {
    expect(
      resolve(
        { apiKey: KEY, release: "argument" },
        { CETRA_RELEASE: "named", GIT_COMMIT: "abc", SOURCE_VERSION: "source" },
      ).release,
    ).toBe("argument");
    expect(resolve({ apiKey: KEY }, { CETRA_RELEASE: "named", GIT_COMMIT: "abc" }).release).toBe(
      "named",
    );
    expect(resolve({ apiKey: KEY }, { GIT_COMMIT: "abc", SOURCE_VERSION: "source" }).release).toBe(
      undefined,
    );
  });
});

describe("serviceName", () => {
  it("reads OTEL_SERVICE_NAME from the env source", () => {
    expect(resolve({ apiKey: KEY }, { OTEL_SERVICE_NAME: "api" }).serviceName).toBe("api");
  });

  it("trims the value and treats an empty one as unset", () => {
    expect(resolve({ apiKey: KEY }, { OTEL_SERVICE_NAME: "  api\t" }).serviceName).toBe("api");
    for (const blank of ["", "   "]) {
      expect(resolve({ apiKey: KEY }, { OTEL_SERVICE_NAME: blank }).serviceName).toBeUndefined();
    }
  });

  it("is undefined when disabled, even with the env var set", () => {
    expect(resolve({ enabled: false }, { OTEL_SERVICE_NAME: "api" }).serviceName).toBeUndefined();
  });
});

describe("mask", () => {
  it("accepts a function mask and rejects anything else", () => {
    const mask = (value: unknown) => value;
    expect(resolve({ apiKey: KEY, mask }).mask).toBe(mask);
    expect(message({ apiKey: KEY, mask: 1 })).toBe("mask: must be callable");
  });
});

describe("useGlobalProvider", () => {
  it("accepts both booleans", () => {
    expect(resolve({ apiKey: KEY, useGlobalProvider: true }).useGlobalProvider).toBe(true);
    expect(resolve({ apiKey: KEY, useGlobalProvider: false }).useGlobalProvider).toBe(false);
  });

  it.each([1, "true", null])("rejects %j", (useGlobalProvider) => {
    expect(message({ apiKey: KEY, useGlobalProvider })).toBe(
      "useGlobalProvider: must be a boolean",
    );
  });

  it("reads no env var", () => {
    const config = resolve({ apiKey: KEY }, { CETRA_USE_GLOBAL_PROVIDER: "true" });
    expect(config.useGlobalProvider).toBe(false);
  });
});

describe("exportFilter", () => {
  it("accepts a function and rejects anything else", () => {
    const exportFilter = (span: ReadableSpan) => span.name === "keep";
    expect(resolve({ apiKey: KEY, exportFilter }).exportFilter).toBe(exportFilter);
    expect(message({ apiKey: KEY, exportFilter: "keep" })).toBe("exportFilter: must be callable");
  });

  it("is ignored, however invalid, when the config is disabled", () => {
    const config = resolve({ enabled: false, useGlobalProvider: "yes", exportFilter: 1 });
    expect(config.enabled).toBe(false);
    expect(config.useGlobalProvider).toBe(false);
    expect(config.exportFilter).toBeUndefined();
  });
});

describe("instrument", () => {
  const instrumentation = {
    setTracerProvider: () => {},
    enable: () => {},
    disable: () => {},
  };

  it("accepts a list of instrumentation objects", () => {
    const config = resolve({ apiKey: KEY, instrument: [instrumentation] });
    expect(config.instrument).toEqual([instrumentation]);
  });

  it("defaults to an empty list", () => {
    expect(resolve({ apiKey: KEY }).instrument).toEqual([]);
  });

  it.each([
    "openai",
    1,
    null,
    [null],
    [{ setTracerProvider: () => {}, enable: () => {} }],
    [{ enable: () => {}, disable: () => {} }],
    [instrumentation, "openai"],
  ])("rejects %j", (instrument) => {
    expect(message({ apiKey: KEY, instrument })).toBe(
      "instrument: must be a list of instrumentation objects",
    );
  });

  it("is empty when the config is disabled, however invalid", () => {
    const config = resolve({ enabled: false, instrument: "openai" });
    expect(config.enabled).toBe(false);
    expect(config.instrument).toEqual([]);
  });
});

describe("process.env", () => {
  it("reads the default env from globalThis.process", () => {
    const previousKey = process.env.CETRA_API_KEY;
    const previousUrl = process.env.CETRA_BASE_URL;
    process.env.CETRA_API_KEY = KEY;
    process.env.CETRA_BASE_URL = "https://from-process.example";
    try {
      expect(resolveConfig().baseUrl).toBe("https://from-process.example");
    } finally {
      if (previousKey === undefined) {
        delete process.env.CETRA_API_KEY;
      } else {
        process.env.CETRA_API_KEY = previousKey;
      }
      if (previousUrl === undefined) {
        delete process.env.CETRA_BASE_URL;
      } else {
        process.env.CETRA_BASE_URL = previousUrl;
      }
    }
  });

  it("does not throw when process itself is missing", () => {
    const warn = suppressWarn();
    const previous = globalThis.process;
    Object.defineProperty(globalThis, "process", { value: undefined, configurable: true });
    try {
      expect(resolveConfig({ apiKey: KEY }).baseUrl).toBe("http://localhost:8080");
      // D-F-3: no env to read, so the key is missing; warn, do not raise.
      expect(resolveConfig().enabled).toBe(false);
      expect(warn).toHaveBeenCalledTimes(1);
    } finally {
      Object.defineProperty(globalThis, "process", { value: previous, configurable: true });
      warn.mockRestore();
    }
  });
});
