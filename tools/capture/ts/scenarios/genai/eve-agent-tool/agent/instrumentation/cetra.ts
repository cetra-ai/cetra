import { OTLPHttpProtoTraceExporter } from "@vercel/otel";
import { otelIntegration } from "eve/instrumentation/otel";

// Plain OTLP/HTTP to the capture receiver's traces route, without an auth header.
export default otelIntegration({
  traceExporter: new OTLPHttpProtoTraceExporter({
    url: process.env.CETRA_CAPTURE_OTLP_ENDPOINT,
  }),
});
