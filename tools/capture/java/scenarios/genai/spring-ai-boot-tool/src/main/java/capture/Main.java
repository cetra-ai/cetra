package capture;

import cetra.capture.Capture;
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
 * Spring Boot 4.1.2 with Spring AI 2.0.2 (Micrometer route, IN8.3): one ChatClient
 * call with one tool round against the capture mock. The endpoints and key come from
 * the capture harness; Spring's own OTLP export is configured through default
 * properties set before the context starts (plan detail 7).
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
                "spring.ai.openai.api-key", Capture.mockKey()));
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
