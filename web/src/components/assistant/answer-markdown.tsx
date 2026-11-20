import { Link } from "@tanstack/react-router";
import ReactMarkdown, { type Components } from "react-markdown";
import rehypeHighlight from "rehype-highlight";
import remarkGfm from "remark-gfm";
import { markdownComponents } from "@/components/payload/markdown";

const LINK_CLASS = "text-primary underline underline-offset-2";

/** App paths open in the same tab; other web and mail links show their URL (D-AS-7). */
const components: Components = {
  ...markdownComponents,
  a: ({ href, children }) => {
    if (href !== undefined && href.startsWith("/") && !href.startsWith("//")) {
      return (
        <Link to={href} className={LINK_CLASS}>
          {children}
        </Link>
      );
    }
    if (href !== undefined && /^(https?:|mailto:)/i.test(href)) {
      return (
        <>
          <a href={href} target="_blank" rel="noopener noreferrer" className={LINK_CLASS}>
            {children}
          </a>{" "}
          (<span className="text-muted-foreground">{href}</span>)
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
