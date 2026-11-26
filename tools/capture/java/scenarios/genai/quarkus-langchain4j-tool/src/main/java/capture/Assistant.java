package capture;

import io.quarkiverse.langchain4j.RegisterAiService;

@RegisterAiService(tools = WeatherTools.class)
public interface Assistant {
    String chat(String message);
}
