package m8

import ai.koog.agents.core.agent.AIAgent
import ai.koog.agents.core.tools.SimpleTool
import ai.koog.agents.core.tools.ToolRegistry
import ai.koog.agents.features.opentelemetry.feature.OpenTelemetry
import ai.koog.prompt.executor.clients.openai.OpenAIClientSettings
import ai.koog.prompt.executor.clients.openai.OpenAILLMClient
import ai.koog.prompt.executor.clients.openai.OpenAIModels
import ai.koog.prompt.executor.llms.MultiLLMPromptExecutor
import ai.koog.serialization.typeToken
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

/** M8 spike: Koog 1.3.0 agent against the fake provider (M8_BASE_URL), OpenTelemetry feature with an OTLP/HTTP exporter. */
fun main(args: Array<String>) = runBlocking {
    val base = System.getenv("M8_BASE_URL")
    val verbose = args.firstOrNull() == "verbose"
    val client = OpenAILLMClient(
        apiKey = "sk-fake-m8",
        settings = OpenAIClientSettings(baseUrl = base),
    )
    val agent = AIAgent(
        promptExecutor = MultiLLMPromptExecutor(client),
        llmModel = OpenAIModels.Chat.GPT4oMini,
        systemPrompt = "You are a helpful assistant.",
        toolRegistry = ToolRegistry { tool(WeatherTool()) },
    ) {
        install(OpenTelemetry) {
            setServiceInfo("m8-koog", "0.0.0")
            // Synchronous export (docs: addSpanProcessor { simpleSpanProcessor(...) }). addSpanExporter (the default,
            // batched) exported nothing before the process exited in run koog-1.
            val exporter = OtlpHttpSpanExporter.builder()
                .setEndpoint("$base/v1/traces")
                .addHeader("Authorization", "Bearer sk-fake-m8")
                .build()
            addSpanProcessor { simpleSpanProcessor(exporter.toOtelKotlinSpanExporter()) }
            setVerbose(verbose)
        }
    }
    println("answer: " + agent.run("What is the weather in Lyon? Use the tool."))
    agent.close()
}
