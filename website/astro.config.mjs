import sitemap from "@astrojs/sitemap";
import starlight from "@astrojs/starlight";
import { defineConfig } from "astro/config";
import starlightLinksValidator from "starlight-links-validator";
import starlightLlmsTxt from "starlight-llms-txt";
import starlightOpenAPI, { openAPISidebarGroups } from "starlight-openapi";
import starlightSidebarTopics from "starlight-sidebar-topics";
import { ALWAYS_NOINDEX } from "./src/marketing/routes.mjs";

// The site is cetra.sh: the landing page at `/` (src/pages/index.astro) and the
// documentation under `/docs/` (src/content/docs/docs/), D-W-2.
export default defineConfig({
  site: "https://cetra.sh",
  trailingSlash: "always",
  integrations: [
    // Starlight adds its own sitemap unless one is present; this one leaves out noindex pages.
    sitemap({
      filter: (page) => !ALWAYS_NOINDEX.includes(new URL(page).pathname),
    }),
    starlight({
      title: "cetra",
      description:
        "Open-source LLM observability and evaluation that runs as one binary on one machine.",
      favicon: "/favicon.svg",
      customCss: ["./src/styles/custom.css"],
      components: {
        // "On this page" as wide as the left sidebar (Starlight's grows with the window).
        TwoColumnContent: "./src/components/TwoColumnContent.astro",
        // Title actions: copy or view the page as Markdown (D-AO-12).
        PageTitle: "./src/components/PageTitle.astro",
      },
      social: [{ icon: "github", label: "GitHub", href: "https://github.com/cetra-ai/cetra" }],
      editLink: { baseUrl: "https://github.com/cetra-ai/cetra/edit/main/website/" },
      // CI checks out one commit; git dates would all be the same.
      lastUpdated: false,
      plugins: [
        starlightLinksValidator(),
        starlightLlmsTxt({
          projectName: "cetra",
          description:
            "Open-source LLM observability and evaluation that runs as one binary on one machine. Traces over OpenTelemetry, scores, evaluators, datasets, experiments and alerts.",
        }),
        starlightOpenAPI([
          {
            base: "docs/reference/api",
            schema: "./src/generated/openapi.json",
            sidebar: { label: "Endpoints", collapsed: true },
          },
        ]),
        starlightSidebarTopics(
          [
            {
              label: "Docs",
              link: "/docs/",
              icon: "open-book",
              items: [
                { label: "Overview", link: "/docs/" },
                {
                  label: "Get started",
                  items: [
                    {
                      label: "Set up with your coding agent",
                      link: "/docs/get-started/coding-agent/",
                    },
                    { label: "Quick start", link: "/docs/get-started/quickstart/" },
                    { label: "Run cetra", link: "/docs/get-started/run-cetra/" },
                    { label: "Send OpenTelemetry", link: "/docs/get-started/opentelemetry/" },
                    { label: "Tour of the UI", link: "/docs/get-started/ui-tour/" },
                  ],
                },
                {
                  label: "Concepts",
                  items: [
                    { label: "Data model", link: "/docs/concepts/data-model/" },
                    { label: "How cetra works", link: "/docs/concepts/how-cetra-works/" },
                    { label: "Semantic conventions", link: "/docs/concepts/conventions/" },
                    { label: "Glossary", link: "/docs/concepts/glossary/" },
                  ],
                },
                {
                  label: "Tracing",
                  items: [
                    { label: "Traces", link: "/docs/tracing/traces/" },
                    { label: "Sessions", link: "/docs/tracing/sessions/" },
                    { label: "Users", link: "/docs/tracing/users/" },
                    { label: "Metadata and tags", link: "/docs/tracing/metadata-and-tags/" },
                    { label: "Filters and search", link: "/docs/tracing/filters-and-search/" },
                    { label: "SQL console", link: "/docs/tracing/sql-console/" },
                    { label: "Dashboard", link: "/docs/tracing/dashboard/" },
                    { label: "Custom dashboards", link: "/docs/tracing/custom-dashboards/" },
                    { label: "Cost and tokens", link: "/docs/tracing/cost-and-tokens/" },
                    { label: "Media", link: "/docs/tracing/media/" },
                    {
                      label: "Masking, sampling and truncation",
                      link: "/docs/tracing/privacy-controls/",
                    },
                  ],
                },
                {
                  label: "Evaluation",
                  items: [
                    { label: "Overview", link: "/docs/evaluation/" },
                    { label: "Scores", link: "/docs/evaluation/scores/" },
                    { label: "Human annotation", link: "/docs/evaluation/annotation/" },
                    { label: "Annotation queues", link: "/docs/evaluation/annotation-queues/" },
                    { label: "Evaluators and LLM judges", link: "/docs/evaluation/evaluators/" },
                    { label: "LLM connections", link: "/docs/evaluation/llm-connections/" },
                    { label: "Online rules", link: "/docs/evaluation/online-rules/" },
                    { label: "Issues", link: "/docs/evaluation/issues/" },
                    { label: "Datasets", link: "/docs/evaluation/datasets/" },
                    { label: "Prompts", link: "/docs/evaluation/prompts/" },
                    { label: "Playground", link: "/docs/evaluation/playground/" },
                    { label: "Experiments", link: "/docs/evaluation/experiments/" },
                  ],
                },
                {
                  label: "Platform",
                  items: [
                    { label: "Alerts", link: "/docs/platform/alerts/" },
                    { label: "MCP server", link: "/docs/platform/mcp/" },
                    { label: "Assistant", link: "/docs/platform/assistant/" },
                    { label: "API keys and tokens", link: "/docs/platform/api-keys/" },
                    { label: "Organizations and roles", link: "/docs/platform/organizations/" },
                    { label: "Single sign-on", link: "/docs/platform/sso/" },
                    {
                      label: "Deletion and retention",
                      link: "/docs/platform/deletion-and-retention/",
                    },
                    { label: "Plans and licence", link: "/docs/platform/plans-and-licence/" },
                  ],
                },
                {
                  label: "Help",
                  items: [
                    { label: "FAQ", link: "/docs/help/faq/" },
                    { label: "Troubleshooting", link: "/docs/help/troubleshooting/" },
                    { label: "Changelog", link: "/docs/help/changelog/" },
                    { label: "Support and security", link: "/docs/help/support/" },
                  ],
                },
              ],
            },
            {
              label: "Self-hosting",
              link: "/docs/self-hosting/",
              icon: "setting",
              items: [
                { label: "Overview", link: "/docs/self-hosting/" },
                {
                  label: "Install",
                  items: [
                    { label: "Install", link: "/docs/self-hosting/install/" },
                    { label: "Docker Compose", link: "/docs/self-hosting/docker-compose/" },
                    { label: "Kubernetes", link: "/docs/self-hosting/kubernetes/" },
                  ],
                },
                {
                  label: "Configure",
                  items: [
                    { label: "Configuration", link: "/docs/self-hosting/configuration/" },
                    {
                      label: "Configuration reference",
                      link: "/docs/self-hosting/configuration-reference/",
                    },
                    { label: "Authentication", link: "/docs/self-hosting/authentication/" },
                    { label: "Email (SMTP)", link: "/docs/self-hosting/email/" },
                    { label: "Object storage (S3)", link: "/docs/self-hosting/s3/" },
                    { label: "Security", link: "/docs/self-hosting/security/" },
                    { label: "Resources", link: "/docs/self-hosting/resources/" },
                  ],
                },
                {
                  label: "Operate",
                  items: [
                    { label: "Backup and restore", link: "/docs/self-hosting/backup/" },
                    { label: "Monitoring", link: "/docs/self-hosting/monitoring/" },
                    { label: "Upgrade", link: "/docs/self-hosting/upgrade/" },
                  ],
                },
              ],
            },
            {
              label: "Integrations",
              link: "/docs/integrations/",
              icon: "puzzle",
              items: [
                { label: "All integrations", link: "/docs/integrations/" },
                {
                  label: "Providers",
                  collapsed: true,
                  items: [
                    { label: "Amazon Bedrock", link: "/docs/integrations/bedrock/" },
                    { label: "Anthropic", link: "/docs/integrations/anthropic/" },
                    { label: "Azure OpenAI", link: "/docs/integrations/azure-openai/" },
                    { label: "Baidu Qianfan", link: "/docs/integrations/qianfan/" },
                    { label: "Baseten", link: "/docs/integrations/baseten/" },
                    { label: "BytePlus ModelArk", link: "/docs/integrations/byteplus/" },
                    { label: "Cerebras", link: "/docs/integrations/cerebras/" },
                    {
                      label: "Cloudflare Workers AI",
                      link: "/docs/integrations/cloudflare-workers-ai/",
                    },
                    { label: "Cohere", link: "/docs/integrations/cohere/" },
                    { label: "CometAPI", link: "/docs/integrations/cometapi/" },
                    { label: "Databricks", link: "/docs/integrations/databricks/" },
                    { label: "DeepInfra", link: "/docs/integrations/deepinfra/" },
                    { label: "DeepSeek", link: "/docs/integrations/deepseek/" },
                    { label: "Doubleword", link: "/docs/integrations/doubleword/" },
                    { label: "Fireworks AI", link: "/docs/integrations/fireworks/" },
                    { label: "Google Gen AI", link: "/docs/integrations/google-genai/" },
                    { label: "Groq", link: "/docs/integrations/groq/" },
                    {
                      label: "Hugging Face Inference Providers",
                      link: "/docs/integrations/hugging-face/",
                    },
                    { label: "Hyperbolic", link: "/docs/integrations/hyperbolic/" },
                    { label: "IBM watsonx.ai", link: "/docs/integrations/watsonx/" },
                    { label: "LiteLLM", link: "/docs/integrations/litellm/" },
                    { label: "MiniMax", link: "/docs/integrations/minimax/" },
                    { label: "Mistral AI", link: "/docs/integrations/mistral/" },
                    { label: "Moonshot AI (Kimi)", link: "/docs/integrations/moonshot/" },
                    { label: "Nebius AI Studio", link: "/docs/integrations/nebius/" },
                    { label: "Novita AI", link: "/docs/integrations/novita/" },
                    { label: "NVIDIA NIM", link: "/docs/integrations/nvidia-nim/" },
                    { label: "Ollama", link: "/docs/integrations/ollama/" },
                    { label: "OpenAI", link: "/docs/integrations/openai/" },
                    { label: "OpenRouter", link: "/docs/integrations/openrouter/" },
                    { label: "Perplexity", link: "/docs/integrations/perplexity/" },
                    {
                      label: "Qwen (Alibaba Cloud Model Studio)",
                      link: "/docs/integrations/qwen/",
                    },
                    { label: "SambaNova", link: "/docs/integrations/sambanova/" },
                    { label: "Sarvam AI", link: "/docs/integrations/sarvam/" },
                    { label: "Together AI", link: "/docs/integrations/together/" },
                    { label: "Vertex AI", link: "/docs/integrations/vertex-ai/" },
                    { label: "vLLM", link: "/docs/integrations/vllm/" },
                    { label: "Volcengine Ark", link: "/docs/integrations/volcengine/" },
                    { label: "xAI", link: "/docs/integrations/xai/" },
                    { label: "Z.ai (GLM)", link: "/docs/integrations/z-ai/" },
                  ],
                },
                {
                  label: "Gateways",
                  collapsed: true,
                  items: [
                    { label: "Bifrost", link: "/docs/integrations/bifrost/" },
                    {
                      label: "Cloudflare AI Gateway",
                      link: "/docs/integrations/cloudflare-ai-gateway/",
                    },
                    { label: "Envoy AI Gateway", link: "/docs/integrations/envoy-ai-gateway/" },
                    { label: "Helicone", link: "/docs/integrations/helicone/" },
                    { label: "Kong AI Gateway", link: "/docs/integrations/kong/" },
                    { label: "LiteLLM Proxy", link: "/docs/integrations/litellm-proxy/" },
                    { label: "Portkey", link: "/docs/integrations/portkey/" },
                    { label: "TrueFoundry", link: "/docs/integrations/truefoundry/" },
                    { label: "Vercel AI Gateway", link: "/docs/integrations/vercel-ai-gateway/" },
                  ],
                },
                {
                  label: "Frameworks",
                  items: [
                    { label: "AG2", link: "/docs/integrations/ag2/" },
                    { label: "Agno", link: "/docs/integrations/agno/" },
                    { label: "aisuite", link: "/docs/integrations/aisuite/" },
                    { label: "AutoGen", link: "/docs/integrations/autogen/" },
                    { label: "Bedrock AgentCore", link: "/docs/integrations/bedrock-agentcore/" },
                    { label: "BeeAI Framework", link: "/docs/integrations/beeai/" },
                    { label: "Claude Agent SDK", link: "/docs/integrations/claude-agent-sdk/" },
                    { label: "CrewAI", link: "/docs/integrations/crewai/" },
                    { label: "Deep Agents", link: "/docs/integrations/deepagents/" },
                    { label: "DSPy", link: "/docs/integrations/dspy/" },
                    { label: "Google ADK", link: "/docs/integrations/google-adk/" },
                    { label: "Guardrails AI", link: "/docs/integrations/guardrails/" },
                    { label: "Haystack", link: "/docs/integrations/haystack/" },
                    { label: "Instructor", link: "/docs/integrations/instructor/" },
                    { label: "LangChain", link: "/docs/integrations/langchain/" },
                    { label: "LangGraph", link: "/docs/integrations/langgraph/" },
                    { label: "LlamaIndex", link: "/docs/integrations/llama-index/" },
                    { label: "Mastra", link: "/docs/integrations/mastra/" },
                    {
                      label: "Microsoft Agent Framework",
                      link: "/docs/integrations/agent-framework/",
                    },
                    { label: "Mirascope", link: "/docs/integrations/mirascope/" },
                    { label: "OpenAI Agents SDK", link: "/docs/integrations/openai-agents/" },
                    { label: "OpenHands", link: "/docs/integrations/openhands/" },
                    { label: "Pydantic AI", link: "/docs/integrations/pydantic-ai/" },
                    { label: "Semantic Kernel", link: "/docs/integrations/semantic-kernel/" },
                    { label: "smolagents", link: "/docs/integrations/smolagents/" },
                    { label: "Strands Agents", link: "/docs/integrations/strands/" },
                    { label: "Temporal", link: "/docs/integrations/temporal/" },
                    { label: "Vercel AI SDK", link: "/docs/integrations/vercel-ai-sdk/" },
                    { label: "VoltAgent", link: "/docs/integrations/voltagent/" },
                  ],
                },
                {
                  label: "Coding agents",
                  items: [
                    { label: "Claude Code", link: "/docs/integrations/claude-code/" },
                    { label: "Codex CLI", link: "/docs/integrations/codex/" },
                    { label: "Gemini CLI", link: "/docs/integrations/gemini-cli/" },
                    { label: "GitHub Copilot", link: "/docs/integrations/copilot/" },
                  ],
                },
                {
                  label: "Instrumentation libraries",
                  items: [
                    { label: "OpenInference", link: "/docs/integrations/openinference/" },
                    { label: "OpenLLMetry", link: "/docs/integrations/openllmetry/" },
                    {
                      label: "OpenTelemetry GenAI",
                      link: "/docs/integrations/opentelemetry-genai/",
                    },
                  ],
                },
                {
                  label: "Migrating",
                  items: [
                    { label: "From Langfuse", link: "/docs/integrations/from-langfuse/" },
                    { label: "From Laminar", link: "/docs/integrations/from-laminar/" },
                  ],
                },
              ],
            },
            {
              label: "Reference",
              id: "reference",
              link: "/docs/reference/",
              icon: "document",
              items: [
                { label: "Overview", link: "/docs/reference/" },
                {
                  label: "Python SDK",
                  items: [
                    { label: "Install and configure", link: "/docs/reference/python/" },
                    { label: "Tracing", link: "/docs/reference/python/tracing/" },
                    { label: "Context", link: "/docs/reference/python/context/" },
                    {
                      label: "Scores, datasets and experiments",
                      link: "/docs/reference/python/scores-datasets-experiments/",
                    },
                    { label: "OpenTelemetry", link: "/docs/reference/python/opentelemetry/" },
                    { label: "API", link: "/docs/reference/python/api/" },
                  ],
                },
                {
                  label: "TypeScript SDK",
                  items: [
                    { label: "Install and configure", link: "/docs/reference/typescript/" },
                    { label: "Tracing", link: "/docs/reference/typescript/tracing/" },
                    { label: "Context", link: "/docs/reference/typescript/context/" },
                    {
                      label: "Scores, datasets and experiments",
                      link: "/docs/reference/typescript/scores-datasets-experiments/",
                    },
                    { label: "OpenTelemetry", link: "/docs/reference/typescript/opentelemetry/" },
                    { label: "Runtimes", link: "/docs/reference/typescript/runtimes/" },
                    { label: "API", link: "/docs/reference/typescript/api/" },
                  ],
                },
                {
                  label: "REST API",
                  items: [
                    { label: "Overview", link: "/docs/reference/api/overview/" },
                    ...openAPISidebarGroups,
                  ],
                },
                { label: "OTLP endpoint", link: "/docs/reference/otlp/" },
                { label: "CLI", link: "/docs/reference/cli/" },
                { label: "cetra-cli", link: "/docs/reference/cetra-cli/" },
                { label: "Span attributes", link: "/docs/reference/attributes/" },
                { label: "Filter language", link: "/docs/reference/filter-language/" },
                { label: "SQL schema", link: "/docs/reference/sql-schema/" },
              ],
            },
          ],
          { topics: { reference: ["/docs/reference/api/**"] } },
        ),
      ],
    }),
  ],
});
