package m8;

import com.arize.instrumentation.OITracer;
import com.arize.instrumentation.TraceConfig;
import com.arize.instrumentation.springAI.SpringAIInstrumentor;
import io.micrometer.observation.Observation;
import io.micrometer.observation.ObservationHandler;
import io.opentelemetry.api.OpenTelemetry;
import org.springframework.ai.chat.client.ChatClient;
import org.springframework.ai.openai.OpenAiChatOptions;
import org.springframework.ai.tool.annotation.Tool;
import org.springframework.ai.tool.annotation.ToolParam;
import org.springframework.boot.ApplicationRunner;
import org.springframework.boot.SpringApplication;
import org.springframework.boot.WebApplicationType;
import org.springframework.boot.autoconfigure.SpringBootApplication;
import org.springframework.boot.autoconfigure.condition.ConditionalOnProperty;
import org.springframework.boot.builder.SpringApplicationBuilder;
import org.springframework.context.annotation.Bean;
import org.springframework.context.annotation.Configuration;

/**
 * M8 follow-up Q2: Spring Boot app, one ChatClient call with one tool round, against the fake provider.
 * Run with -Dm8.oi=true to register OpenInference's SpringAIInstrumentor as an ObservationHandler bean.
 * Exits when the single ApplicationRunner returns (SpringApplication.exit).
 */
@SpringBootApplication
public class BootRun {

    public static void main(String[] args) {
        if (System.getProperty("m8.hold") != null) {
            // Experiment: no SpringApplication.exit and no System.exit; does the JVM end by itself?
            SpringApplication.run(BootRun.class, args);
            return;
        }
        int code = SpringApplication.exit(
                new SpringApplicationBuilder(BootRun.class).web(WebApplicationType.NONE).run(args));
        System.exit(code);
    }

    public static class WeatherTools {
        @Tool(description = "Get the current weather for a city")
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
        @ConditionalOnProperty(name = "m8.oi", havingValue = "true")
        ObservationHandler<Observation.Context> openInferenceHandler(OpenTelemetry otel) {
            return new SpringAIInstrumentor(new OITracer(otel.getTracer("m8-boot"), TraceConfig.getDefault()));
        }

        @Bean
        ApplicationRunner oneCall(ChatClient.Builder chat, WeatherTools tools) {
            return args -> {
                String answer = chat.defaultTools(tools).build().prompt()
                        .options(OpenAiChatOptions.builder().model("gpt-4o-mini"))
                        .user("What is the weather in Lyon? Use the tool.")
                        .call()
                        .content();
                System.out.println("answer: " + answer);
            };
        }
    }
}
