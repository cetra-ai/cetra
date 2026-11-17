import { cn } from "cn";
import { isValidElement, type ReactNode } from "react";
import ReactMarkdown, { type Components } from "react-markdown";
import rehypeHighlight from "rehype-highlight";
import remarkGfm from "remark-gfm";
import { CopyButton } from "@/components/copy-button";

/**
 * GitHub-flavoured Markdown with highlighted code. Raw HTML is never parsed: there is no
 * rehype-raw, so HTML in the text is shown as text. Code without a language is not guessed.
 */
export function Markdown({ text }: { text: string }) {
  return (
    <div className="min-w-0 break-words">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        rehypePlugins={[rehypeHighlight]}
        components={components}
      >
        {text}
      </ReactMarkdown>
    </div>
  );
}

const components: Components = {
  h1: ({ children }) => <h1 className="mt-3 mb-1.5 text-[17px] font-semibold">{children}</h1>,
  h2: ({ children }) => <h2 className="mt-3 mb-1.5 text-[15px] font-semibold">{children}</h2>,
  h3: ({ children }) => <h3 className="mt-2 mb-1 text-[13px] font-semibold">{children}</h3>,
  h4: ({ children }) => <h4 className="mt-2 mb-1 text-[13px] font-semibold">{children}</h4>,
  h5: ({ children }) => <h5 className="mt-2 mb-1 text-[13px] font-semibold">{children}</h5>,
  h6: ({ children }) => <h6 className="mt-2 mb-1 text-[13px] font-semibold">{children}</h6>,
  p: ({ children }) => <p className="my-1.5 text-[13px] leading-relaxed">{children}</p>,
  ul: ({ children }) => <ul className="my-1.5 list-disc pl-5">{children}</ul>,
  ol: ({ children }) => <ol className="my-1.5 list-decimal pl-5">{children}</ol>,
  li: ({ children }) => <li className="my-0.5 text-[13px] leading-relaxed">{children}</li>,
  a: ({ href, children }) => (
    <a
      href={href}
      target="_blank"
      rel="noopener noreferrer"
      className="text-primary underline underline-offset-2"
    >
      {children}
    </a>
  ),
  // Fenced code is a `code` inside a `pre`. The `in-[pre]` overrides turn the inline style off there.
  code: ({ className, children }) => (
    <code
      className={cn(
        "rounded bg-muted px-1 py-0.5 font-mono text-[12px] in-[pre]:bg-transparent in-[pre]:p-0",
        className,
      )}
    >
      {children}
    </code>
  ),
  pre: ({ children }) => <CodeBlock>{children}</CodeBlock>,
  table: ({ children }) => (
    <div className="my-2 overflow-auto">
      <table className="w-full border-collapse text-[12px]">{children}</table>
    </div>
  ),
  th: ({ children }) => <th className="border px-2 py-1 text-left">{children}</th>,
  td: ({ children }) => <td className="border px-2 py-1 text-left">{children}</td>,
  blockquote: ({ children }) => (
    <blockquote className="my-2 border-l-2 pl-3 text-muted-foreground">{children}</blockquote>
  ),
  hr: () => <hr className="my-3 border-border" />,
};

function CodeBlock({ children }: { children: ReactNode }) {
  const code = isValidElement<{ className?: string; children?: ReactNode }>(children)
    ? children
    : undefined;
  const language = /language-([\w-]+)/.exec(code?.props.className ?? "")?.[1] ?? "text";
  const text = reactText(code ? code.props.children : children).replace(/\n$/, "");
  return (
    <div className="my-2 overflow-hidden rounded-md border bg-surface">
      <div className="flex items-center justify-between border-b px-3 py-1 text-[11px] text-muted-foreground">
        <span>{language}</span>
        <CopyButton value={text} label="code" />
      </div>
      <pre className="overflow-auto p-3 font-mono text-[12px] leading-relaxed">{children}</pre>
    </div>
  );
}

/** The plain text inside React children, including the spans highlighting adds. */
function reactText(node: ReactNode): string {
  if (typeof node === "string" || typeof node === "number") {
    return String(node);
  }
  if (Array.isArray(node)) {
    return node.map((child: ReactNode) => reactText(child)).join("");
  }
  if (isValidElement<{ children?: ReactNode }>(node)) {
    return reactText(node.props.children);
  }
  return "";
}
