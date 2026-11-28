@file:JvmName("Main")

package capture

import ai.koog.agents.core.agent.AIAgent
import ai.koog.agents.core.tools.SimpleTool
import ai.koog.agents.core.tools.ToolRegistry
import ai.koog.agents.features.opentelemetry.feature.OpenTelemetry
import ai.koog.prompt.executor.clients.openai.OpenAIClientSettings
import ai.koog.prompt.executor.clients.openai.OpenAILLMClient
import ai.koog.prompt.executor.clients.openai.OpenAIModels
import ai.koog.prompt.executor.llms.MultiLLMPromptExecutor
import ai.koog.serialization.typeToken
import cetra.capture.Capture
import io.opentelemetry.exporter.otlp.http.trace.OtlpHttpSpanExporter
import io.opentelemetry.kotlin.tracing.export.simpleSpanProcessor
import io.opentelemetry.kotlin.tracing.export.toOtelKotlinSpanExporter
import kotlinx.coroutines.runBlocking
import kotlinx.serialization.Serializable

@Serializable
data class WeatherArgs(val city: String)

class WeatherTool : SimpleTool<WeatherArgs>(
    argsType = typeToken<WeatherArgs>(),
    name = "get_weather",
    description = "Get the current weather for a city",
) {
    override suspend fun execute(args: WeatherArgs): String = "Sunny in ${args.city}, 21 C"
}

/**
 * Koog 1.3.0 agent with its OpenTelemetry feature (IN8.3, from
 * spikes/m8-files/java/koog): verbose so content is recorded, and a synchronous span
 * processor because the default batch exporter sent nothing before exit
 * (spikes/m8-java.md § 5.4).
 */
fun main(args: Array<String>) = runBlocking {
    val client = OpenAILLMClient(
        apiKey = Capture.mockKey(),
        settings = OpenAIClientSettings(baseUrl = Capture.mockBaseUrl()),
    )
    val agent = AIAgent(
        promptExecutor = MultiLLMPromptExecutor(client),
        llmModel = OpenAIModels.Chat.GPT4oMini,
        systemPrompt = "You are a helpful assistant.",
        toolRegistry = ToolRegistry { tool(WeatherTool()) },
    ) {
        install(OpenTelemetry) {
            val exporter = OtlpHttpSpanExporter.builder()
                .setEndpoint(Capture.otlpTracesEndpoint())
                .build()
            addSpanProcessor { simpleSpanProcessor(exporter.toOtelKotlinSpanExporter()) }
            setVerbose(true)
        }
    }
    println("answer: " + agent.run("What is the weather in Lyon? Use the tool."))
    agent.close()
    Capture.writeMeta()
}
