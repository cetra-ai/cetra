import { ArrowRight } from "lucide-react";
import { Button } from "@/components/ui/button";

export const SIGNUP_URL = "https://app.cetra.sh/signup";

/** The strip above the top bar in the website's demo build (`vite build --mode demo`). */
export function DemoBar() {
  return (
    <div
      role="status"
      className="flex h-11 shrink-0 items-center justify-center gap-3 border-b bg-muted px-3 text-body"
    >
      <span className="truncate text-muted-foreground">
        <span className="sm:hidden">Demo with sample data</span>
        <span className="hidden sm:inline">You're exploring a demo with sample data.</span>
      </span>
      <Button asChild size="sm">
        <a href={SIGNUP_URL} target="_blank" rel="noopener noreferrer">
          Start free
          <ArrowRight data-icon="inline-end" />
        </a>
      </Button>
    </div>
  );
}
