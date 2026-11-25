import { OTLPHttpProtoTraceExporter } from "@vercel/otel";
import { otelIntegration } from "eve/instrumentation/otel";

// M9 spike: plain OTLP/HTTP to cetra's route (no OpenInference processor, as the Phoenix page adds).
export default otelIntegration({
  traceExporter: new OTLPHttpProtoTraceExporter({
    url: process.env.M9_OTLP!,
    headers: { authorization: "Bearer sk-cetra-fake-m9" },
  }),
});
