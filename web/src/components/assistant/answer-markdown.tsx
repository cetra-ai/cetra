import { Link } from "@tanstack/react-router";
import ReactMarkdown, { type Components } from "react-markdown";
import rehypeHighlight from "rehype-highlight";
import remarkGfm from "remark-gfm";
import { markdownComponents } from "@/components/payload/markdown";

const LINK_CLASS = "text-primary underline underline-offset-2";

/** Whether `s` has a backslash, whitespace or a control character. */
function hasUnsafeChar(s: string): boolean {
  return /[\\\s]/.test(s) || [...s].some((c) => c.charCodeAt(0) < 0x20 || c.charCodeAt(0) === 0x7f);
}

/**
 * The in-app path of a same-origin `href` that starts with one `/`, or `undefined`. A backslash,
 * whitespace or control character is refused in the raw href and in its percent-decoded form,
 * since a browser may read `/\\host` or `/%09/host` as another host (D-AS-7).
 */
function appPath(href: string): string | undefined {
  if (!href.startsWith("/") || href.startsWith("//") || hasUnsafeChar(href)) {
    return undefined;
  }
  try {
    if (hasUnsafeChar(decodeURIComponent(href))) {
      return undefined;
    }
    const url = new URL(href, window.location.origin);
    if (url.origin !== window.location.origin) {
      return undefined;
    }
    return url.pathname + url.search + url.hash;
  } catch {
    return undefined;
  }
}

/** The absolute web or mail link, when `href` starts with its scheme and parses, or `undefined`. */
function externalHref(href: string): string | undefined {
  if (!/^(https?:|mailto:)/i.test(href)) {
    return undefined;
  }
  try {
    new URL(href);
    return href;
  } catch {
    return undefined;
  }
}

/** App paths open in the same tab; other web and mail links show their URL (D-AS-7). */
const components: Components = {
  ...markdownComponents,
  a: ({ href, children }) => {
    const path = href === undefined ? undefined : appPath(href);
    if (path !== undefined) {
      return (
        <Link to={path} className={LINK_CLASS}>
          {children}
        </Link>
      );
    }
    const external = href === undefined ? undefined : externalHref(href);
    if (external !== undefined) {
      return (
        <>
          <a href={external} target="_blank" rel="noopener noreferrer" className={LINK_CLASS}>
            {children}
          </a>{" "}
          (<span className="text-muted-foreground">{external}</span>)
        </>
      );
    }
    return <>{children}</>;
  },
};

/**
 * An assistant answer as Markdown. Images are dropped, and raw HTML is shown as text, since there
 * is no rehype-raw. Re-rendered with the growing text while a turn streams.
 */
export function AnswerMarkdown({ text }: { text: string }) {
  return (
    <div className="min-w-0 break-words">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        rehypePlugins={[rehypeHighlight]}
        disallowedElements={["img"]}
        unwrapDisallowed
        components={components}
      >
        {text}
      </ReactMarkdown>
    </div>
  );
}
