package capture;

import cetra.capture.Capture;
import com.arize.instrumentation.langchain4j.LangChain4jInstrumentor;
import dev.langchain4j.agent.tool.P;
import dev.langchain4j.agent.tool.Tool;
import dev.langchain4j.model.chat.listener.ChatModelListener;
import dev.langchain4j.model.openai.OpenAiChatModel;
import dev.langchain4j.service.AiServices;
import io.opentelemetry.sdk.OpenTelemetrySdk;
import io.opentelemetry.sdk.autoconfigure.AutoConfiguredOpenTelemetrySdk;
import java.util.List;
import java.util.concurrent.TimeUnit;

/**
 * LangChain4j 1.23.0 through OpenInference Java 0.1.9, model listener only (IN8.3):
 * the AiService listeners are not registered, so each model call is one llm span and
 * there is no agent or tool span. The SDK is autoconfigured from the runner's OTEL_*
 * environment; the metrics and logs exporters are turned off in the scenario.
 */
public class Main {

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
        OpenTelemetrySdk otel = AutoConfiguredOpenTelemetrySdk.initialize().getOpenTelemetrySdk();
        LangChain4jInstrumentor instrumentor =
                LangChain4jInstrumentor.instrument(otel.getTracerProvider());
        OpenAiChatModel model = OpenAiChatModel.builder()
                .baseUrl(Capture.mockBaseUrl() + "/v1")
                .apiKey(Capture.mockKey())
                .modelName("gpt-4o-mini")
                .listeners(List.of(instrumentor.createModelListener()))
                .build();
        Assistant assistant = AiServices.builder(Assistant.class)
                .chatModel(model)
                .tools(new WeatherTools())
                .build();
        System.out.println("answer: " + assistant.chat("What is the weather in Lyon? Use the tool."));
        otel.getSdkTracerProvider().forceFlush().join(10, TimeUnit.SECONDS);
        otel.close();
        Capture.writeMeta();
    }
}
