import { type DialectOptions, formatDialect, postgresql } from "sql-formatter";

/**
 * `{name:Type}`, the placeholder grammar of `sqlParams` in `params.ts`. Each word is
 * `[\p{Alphabetic}_#@][\p{Alphabetic}0-9@$#_]*`; sql-formatter wraps the pattern in `uy` flags.
 */
const WORD = "[\\p{Alphabetic}_#@][\\p{Alphabetic}0-9@$#_]*";
const PLACEHOLDER = `\\{${WORD}:${WORD}\\}`;

/** A plain identifier, the only shape a function name can take in the regex sql-formatter builds. */
const FUNCTION_NAME = /^[A-Za-z_][A-Za-z0-9_]*$/;

/** The dialect of the last function list, so Shift-Alt-F does not rebuild it each time. */
let memo: { key: string; dialect: DialectOptions } | undefined;

/**
 * sql-formatter's PostgreSQL dialect with `functionNames` added to its function list. Without
 * them a function it does not know is written with a space before its parenthesis.
 */
function dialectFor(functionNames: readonly string[]): DialectOptions {
  const reserved = postgresql.tokenizerOptions.reservedFunctionNames ?? [];
  const known = new Set(reserved.map((name) => name.toUpperCase()));
  const extra: string[] = [];
  for (const name of functionNames) {
    const upper = name.toUpperCase();
    if (FUNCTION_NAME.test(name) && !known.has(upper)) {
      known.add(upper);
      extra.push(name);
    }
  }
  const key = extra
    .map((name) => name.toUpperCase())
    .sort()
    .join("\n");
  if (memo?.key !== key) {
    memo = {
      key,
      dialect: {
        ...postgresql,
        tokenizerOptions: {
          ...postgresql.tokenizerOptions,
          reservedFunctionNames: [...reserved, ...extra],
        },
      },
    };
  }
  return memo.dialect;
}

/**
 * The SQL with keywords upper-cased and one clause per line. Placeholders are kept as written,
 * and a text that does not parse is returned unchanged.
 */
export function formatSql(text: string, functionNames: readonly string[] = []): string {
  if (text.trim() === "") {
    return text;
  }
  try {
    return formatDialect(text, {
      dialect: dialectFor(functionNames),
      keywordCase: "upper",
      tabWidth: 2,
      linesBetweenQueries: 1,
      paramTypes: { custom: [{ regex: PLACEHOLDER }] },
    });
  } catch {
    return text;
  }
}
