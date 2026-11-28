package m8;

import com.arize.instrumentation.OITracer;
import com.arize.instrumentation.TraceConfig;
import com.arize.instrumentation.springAI.SpringAIInstrumentor;
import com.openai.client.OpenAIClient;
import com.openai.client.okhttp.OpenAIOkHttpClient;
import io.micrometer.observation.ObservationRegistry;
import io.micrometer.tracing.handler.DefaultTracingObservationHandler;
import io.micrometer.tracing.otel.bridge.OtelCurrentTraceContext;
import io.micrometer.tracing.otel.bridge.OtelTracer;
import io.opentelemetry.api.trace.Tracer;
import io.opentelemetry.sdk.OpenTelemetrySdk;
import io.opentelemetry.sdk.autoconfigure.AutoConfiguredOpenTelemetrySdk;
import org.springframework.ai.chat.client.ChatClient;
import org.springframework.ai.openai.OpenAiChatModel;
import org.springframework.ai.openai.OpenAiChatOptions;
import org.springframework.ai.tool.ToolCallback;
import org.springframework.ai.tool.function.FunctionToolCallback;

/**
 * M8 follow-up Q1: OpenInference's Spring AI instrumentor (ObservationHandler) on a Micrometer registry,
 * against Spring AI 2.0.2 and the fake provider. Mode (args[0]):
 *   oi      = SpringAIInstrumentor only (the Phoenix page's wiring)
 *   both    = Micrometer's DefaultTracingObservationHandler + SpringAIInstrumentor on one registry
 *   spring  = Micrometer's DefaultTracingObservationHandler only (Spring AI's own spans)
 * OTel SDK from AutoConfiguredOpenTelemetrySdk (OTEL_EXPORTER_OTLP_* env route), as in the Java spike.
 */
public class OiSpringRun {

    public record WeatherRequest(String city) {}

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "oi";
        String base = System.getenv("M8_BASE_URL");
        OpenTelemetrySdk otel = AutoConfiguredOpenTelemetrySdk.initialize().getOpenTelemetrySdk();
        Tracer tracer = otel.getTracer("m8-oi-spring");

        ObservationRegistry registry = ObservationRegistry.create();
        if (mode.equals("both") || mode.equals("spring")) {
            registry.observationConfig().observationHandler(
                    new DefaultTracingObservationHandler(new OtelTracer(tracer, new OtelCurrentTraceContext(), event -> { })));
        }
        if (mode.equals("both") || mode.equals("oi")) {
            TraceConfig cfg = System.getenv("M8_HIDE") != null
                    ? TraceConfig.builder().hideInputs(true).hideOutputs(true).build()
                    : TraceConfig.getDefault();
            OITracer oiTracer = new OITracer(tracer, cfg);
            registry.observationConfig().observationHandler(new SpringAIInstrumentor(oiTracer));
        }

        OpenAIClient client = OpenAIOkHttpClient.builder()
                .baseUrl(base + "/v1")
                .apiKey(System.getenv("OPENAI_API_KEY"))
                .build();
        OpenAiChatModel model = OpenAiChatModel.builder()
                .openAiClient(client)
                .observationRegistry(registry)
                .build();

        ToolCallback weather = FunctionToolCallback
                .builder("get_weather", (WeatherRequest r) -> "Sunny in " + r.city() + ", 21 C")
                .description("Get the current weather for a city")
                .inputType(WeatherRequest.class)
                .build();

        ChatClient chat = ChatClient.builder(model, registry, null, null)
                .defaultToolCallbacks(weather)
                .build();

        io.opentelemetry.api.trace.Span appSpan = null;
        io.opentelemetry.context.Scope appScope = null;
        if (System.getenv("M8_APP_SPAN") != null) {
            // The app opens its own span and makes it current, to see where the OI spans attach.
            appSpan = tracer.spanBuilder("app.request").startSpan();
            appScope = appSpan.makeCurrent();
        }
        String answer = chat.prompt()
                .options(OpenAiChatOptions.builder().model("gpt-4o-mini"))
                .user("What is the weather in Lyon? Use the tool.")
                .call()
                .content();
        if (appSpan != null) {
            appScope.close();
            appSpan.end();
        }
        System.out.println("answer: " + answer);

        otel.getSdkTracerProvider().forceFlush().join(10, java.util.concurrent.TimeUnit.SECONDS);
        otel.close();
    }
}
