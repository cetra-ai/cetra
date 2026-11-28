package m8;

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
 * M8 spike: Spring AI 2.0.2 (plain Java, no Spring Boot). OpenAI chat model against the fake provider
 * (base URL in M8_BASE_URL). Tool calls run through ChatClient, whose default ToolCallingAdvisor executes them.
 * The OTel SDK is configured by AutoConfiguredOpenTelemetrySdk, i.e. from OTEL_EXPORTER_OTLP_* environment variables.
 */
public class SpringAiRun {

    public record WeatherRequest(String city) {}

    public static void main(String[] args) {
        String base = System.getenv("M8_BASE_URL"); // e.g. http://127.0.0.1:PORT (no /v1)
        OpenTelemetrySdk otel = AutoConfiguredOpenTelemetrySdk.initialize().getOpenTelemetrySdk();
        Tracer tracer = otel.getTracer("m8-spring-ai");

        ObservationRegistry registry = ObservationRegistry.create();
        registry.observationConfig().observationHandler(
                new DefaultTracingObservationHandler(new OtelTracer(tracer, new OtelCurrentTraceContext(), event -> { })));

        if (System.getenv("M8_CONTENT_HANDLERS") != null) {
            // Spring AI's content handlers (log-only in 2.0.2): registered only when the program asks for them.
            registry.observationConfig().observationHandler(new org.springframework.ai.chat.observation.ChatModelPromptContentObservationHandler());
            registry.observationConfig().observationHandler(new org.springframework.ai.chat.observation.ChatModelCompletionObservationHandler());
            registry.observationConfig().observationHandler(new org.springframework.ai.chat.client.observation.ChatClientPromptContentObservationHandler());
            registry.observationConfig().observationHandler(new org.springframework.ai.chat.client.observation.ChatClientCompletionObservationHandler());
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

        String answer = chat.prompt()
                .options(OpenAiChatOptions.builder().model("gpt-4o-mini"))
                .user("What is the weather in Lyon? Use the tool.")
                .call()
                .content();
        System.out.println("answer: " + answer);

        otel.getSdkTracerProvider().forceFlush().join(10, java.util.concurrent.TimeUnit.SECONDS);
        otel.close();
    }
}
