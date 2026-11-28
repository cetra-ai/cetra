package m8;

import jakarta.enterprise.context.control.ActivateRequestContext;
import io.quarkus.runtime.QuarkusApplication;
import io.quarkus.runtime.annotations.QuarkusMain;
import jakarta.inject.Inject;

/** M8 spike: one AI service call with one tool round, then the application exits. */
@QuarkusMain
public class QuarkusRun implements QuarkusApplication {

    @Inject
    Assistant assistant;

    @Override
    @ActivateRequestContext
    public int run(String... args) {
        System.out.println("answer: " + assistant.chat("What is the weather in Lyon? Use the tool."));
        return 0;
    }
}
