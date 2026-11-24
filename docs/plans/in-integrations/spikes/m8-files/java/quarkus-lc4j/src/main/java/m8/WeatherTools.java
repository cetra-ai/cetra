package m8;

import dev.langchain4j.agent.tool.P;
import dev.langchain4j.agent.tool.Tool;
import jakarta.enterprise.context.ApplicationScoped;

@ApplicationScoped
public class WeatherTools {
    @Tool("Get the current weather for a city")
    public String get_weather(@P("city") String city) {
        return "Sunny in " + city + ", 21 C";
    }
}
