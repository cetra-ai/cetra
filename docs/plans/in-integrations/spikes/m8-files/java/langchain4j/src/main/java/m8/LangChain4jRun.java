package m8;

import com.arize.instrumentation.langchain4j.LangChain4jInstrumentor;
import dev.langchain4j.agent.tool.P;
import dev.langchain4j.agent.tool.Tool;
import dev.langchain4j.model.chat.listener.ChatModelListener;
import dev.langchain4j.model.openai.OpenAiChatModel;
import dev.langchain4j.observation.listener.ObservationChatModelListener;
import dev.langchain4j.service.AiServices;
import io.micrometer.core.instrument.simple.SimpleMeterRegistry;
import io.micrometer.observation.ObservationRegistry;
import io.micrometer.tracing.handler.DefaultTracingObservationHandler;
import io.micrometer.tracing.otel.bridge.OtelCurrentTraceContext;
import io.micrometer.tracing.otel.bridge.OtelTracer;
import io.opentelemetry.sdk.OpenTelemetrySdk;
import io.opentelemetry.sdk.autoconfigure.AutoConfiguredOpenTelemetrySdk;
import java.util.List;

/**
 * M8 spike: LangChain4j 1.23.0 against the fake provider (M8_BASE_URL). One AiService with one tool.
 * Mode (argument 0): "oi" attaches OpenInference Java's listeners; "native" attaches LangChain4j's own
 * Micrometer observation listener and bridges Micrometer to OTel. OTLP is read from OTEL_EXPORTER_OTLP_* env.
 */
public class LangChain4jRun {

    public interface Assistant {
        String chat(String message);
    }

    public static class WeatherTools {
        @Tool("Get the current weather for a city")
        public String get_weather(@P("city") String city) {
            return "Sunny in " + city + ", 21 C";
        }
    }

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "oi";
        String base = System.getenv("M8_BASE_URL");
        OpenTelemetrySdk otel = AutoConfiguredOpenTelemetrySdk.initialize().getOpenTelemetrySdk();

        List<ChatModelListener> modelListeners;
        List<dev.langchain4j.observability.api.listener.AiServiceListener<?>> serviceListeners;
        if (mode.startsWith("oi")) {
            LangChain4jInstrumentor instrumentor = LangChain4jInstrumentor.instrument(otel.getTracerProvider());
            // "oi" attaches both listener kinds; "oi-model" only the chat model listener; "oi-service" only the AiService listeners.
            modelListeners = mode.equals("oi-service") ? List.of() : List.of(instrumentor.createModelListener());
            serviceListeners = mode.equals("oi-model") ? List.of() : instrumentor.createAiServiceListeners();
        } else {
            ObservationRegistry registry = ObservationRegistry.create();
            registry.observationConfig().observationHandler(new DefaultTracingObservationHandler(
                    new OtelTracer(otel.getTracer("m8-langchain4j"), new OtelCurrentTraceContext(), event -> { })));
            modelListeners = List.of(new ObservationChatModelListener(registry, new SimpleMeterRegistry()));
            serviceListeners = List.of();
        }

        OpenAiChatModel model = OpenAiChatModel.builder()
                .baseUrl(base + "/v1")
                .apiKey(System.getenv("OPENAI_API_KEY"))
                .modelName("gpt-4o-mini")
                .listeners(modelListeners)
                .build();

        Assistant assistant = AiServices.builder(Assistant.class)
                .chatModel(model)
                .tools(new WeatherTools())
                .registerListeners(serviceListeners)
                .build();

        System.out.println("answer: " + assistant.chat("What is the weather in Lyon? Use the tool."));
        otel.getSdkTracerProvider().forceFlush().join(10, java.util.concurrent.TimeUnit.SECONDS);
        otel.close();
    }
}
