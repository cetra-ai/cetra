package capture;

import cetra.capture.Capture;
import io.quarkus.runtime.Quarkus;
import io.quarkus.runtime.QuarkusApplication;
import io.quarkus.runtime.annotations.QuarkusMain;
import jakarta.enterprise.context.control.ActivateRequestContext;
import jakarta.inject.Inject;

/**
 * Quarkus LangChain4j (IN8.3, from spikes/m8-files/java/quarkus-lc4j): one AI service
 * call with one tool round. The mock and receiver endpoints are set as system
 * properties before {@code Quarkus.run} (the base OTLP form the spike verified,
 * spikes/m8-java.md § 5.3); Quarkus flushes its SDK at shutdown, so the meta file is
 * written when {@code run()} returns.
 */
@QuarkusMain
public class Main implements QuarkusApplication {

    @Inject
    Assistant assistant;

    @Override
    @ActivateRequestContext
    public int run(String... args) {
        System.out.println("answer: " + assistant.chat("What is the weather in Lyon? Use the tool."));
        Capture.writeMeta();
        return 0;
    }

    public static void main(String[] args) {
        System.setProperty("quarkus.langchain4j.openai.base-url", Capture.mockBaseUrl() + "/v1");
        System.setProperty("quarkus.otel.exporter.otlp.endpoint", Capture.otlpBaseEndpoint());
        System.setProperty("quarkus.otel.service.name", "capture-app");
        Quarkus.run(Main.class, args);
    }
}
