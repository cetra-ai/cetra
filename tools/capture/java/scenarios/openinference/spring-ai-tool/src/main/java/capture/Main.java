package capture;

import cetra.capture.Capture;
import com.arize.instrumentation.OITracer;
import com.arize.instrumentation.TraceConfig;
import com.arize.instrumentation.springAI.SpringAIInstrumentor;
import io.micrometer.observation.Observation;
import io.micrometer.observation.ObservationHandler;
import io.opentelemetry.api.OpenTelemetry;
import io.opentelemetry.api.trace.Span;
import io.opentelemetry.context.Scope;
import java.util.Map;
import org.springframework.ai.chat.client.ChatClient;
import org.springframework.ai.openai.OpenAiChatOptions;
import org.springframework.ai.tool.annotation.Tool;
import org.springframework.ai.tool.annotation.ToolParam;
import org.springframework.boot.ApplicationRunner;
import org.springframework.boot.SpringApplication;
import org.springframework.boot.WebApplicationType;
import org.springframework.boot.autoconfigure.SpringBootApplication;
import org.springframework.boot.builder.SpringApplicationBuilder;
import org.springframework.context.ConfigurableApplicationContext;
import org.springframework.context.annotation.Bean;
import org.springframework.context.annotation.Configuration;

/**
 * The OpenInference route for the Boot app of genai/spring-ai-boot-tool (IN8.3):
 * OpenInference's Spring AI handler is the only tracing handler — Micrometer's own
 * tracing auto-configuration is excluded (spikes/m8-followup.md § 9.1) — and the
 * ChatClient call runs inside an application span, so both model calls share one
 * trace.
 */
@SpringBootApplication
public class Main {

    public static void main(String[] args) {
        SpringApplication application = new SpringApplicationBuilder(Main.class)
                .web(WebApplicationType.NONE)
                .build();
        application.setDefaultProperties(Map.of(
                "management.opentelemetry.tracing.export.otlp.endpoint",
                Capture.otlpTracesEndpoint(),
                "management.tracing.sampling.probability", "1.0",
                "management.otlp.metrics.export.enabled", "false",
                "spring.ai.openai.base-url", Capture.mockBaseUrl() + "/v1",
                "spring.ai.openai.api-key", Capture.mockKey(),
                "spring.autoconfigure.exclude",
                "org.springframework.boot.micrometer.tracing.autoconfigure"
                        + ".MicrometerTracingAutoConfiguration"));
        ConfigurableApplicationContext context = application.run(args);
        SpringApplication.exit(context);
        Capture.writeMeta();
        System.exit(0);
    }

    public static class WeatherTools {
        @Tool(name = "get_weather", description = "Get the current weather for a city")
        public String getWeather(@ToolParam(description = "City name") String city) {
            return "Sunny in " + city + ", 21 C";
        }
    }

    @Configuration
    static class Wiring {

        @Bean
        WeatherTools weatherTools() {
            return new WeatherTools();
        }

        @Bean
        ObservationHandler<Observation.Context> openInferenceHandler(OpenTelemetry otel) {
            return new SpringAIInstrumentor(
                    new OITracer(otel.getTracer("capture"), TraceConfig.getDefault()));
        }

        @Bean
        ApplicationRunner oneCall(ChatClient.Builder chat, WeatherTools tools, OpenTelemetry otel) {
            return args -> {
                Span appSpan = otel.getTracer("capture").spanBuilder("app.request").startSpan();
                try (Scope ignored = appSpan.makeCurrent()) {
                    String answer = chat.defaultTools(tools).build().prompt()
                            .options(OpenAiChatOptions.builder().model("gpt-4o-mini"))
                            .user("What is the weather in Lyon? Use the tool.")
                            .call()
                            .content();
                    System.out.println("answer: " + answer);
                } finally {
                    appSpan.end();
                }
            };
        }
    }
}
