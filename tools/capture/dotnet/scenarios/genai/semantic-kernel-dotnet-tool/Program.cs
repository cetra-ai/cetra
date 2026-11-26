// Semantic Kernel .NET 1.82.0 (IN8.4, from spikes/m8-files/dotnet/sk): one chat call
// with one tool round against the capture mock, traced by Semantic Kernel's GenAI
// diagnostics with both AppContext switches on, so the chat spans carry their content
// as gen_ai.* events. The endpoints and key come from the capture harness; the OTLP
// endpoint is set as the base URL because OpenTelemetry .NET 1.21.0 ignores
// OTEL_EXPORTER_OTLP_TRACES_ENDPOINT (spikes/m8-followup.md § 9.3).
using System.ComponentModel;
using Cetra.Capture;
using Microsoft.SemanticKernel;
using Microsoft.SemanticKernel.ChatCompletion;
using Microsoft.SemanticKernel.Connectors.OpenAI;
using OpenTelemetry;
using OpenTelemetry.Trace;

AppContext.SetSwitch("Microsoft.SemanticKernel.Experimental.GenAI.EnableOTelDiagnostics", true);
AppContext.SetSwitch("Microsoft.SemanticKernel.Experimental.GenAI.EnableOTelDiagnosticsSensitive", true);

Environment.SetEnvironmentVariable("OTEL_EXPORTER_OTLP_ENDPOINT", Capture.OtlpBaseEndpoint());
using var tracerProvider = Sdk.CreateTracerProviderBuilder()
    .AddSource("Microsoft.SemanticKernel*")
    .AddOtlpExporter()
    .Build();

var builder = Kernel.CreateBuilder();
builder.AddOpenAIChatCompletion(modelId: "gpt-4o-mini", apiKey: Capture.MockKey(),
    endpoint: new Uri(Capture.MockBaseUrl() + "/v1"));
var kernel = builder.Build();
kernel.ImportPluginFromType<WeatherPlugin>();
var chat = kernel.GetRequiredService<IChatCompletionService>();

var settings = new OpenAIPromptExecutionSettings { FunctionChoiceBehavior = FunctionChoiceBehavior.Auto() };
var history = new ChatHistory();
history.AddUserMessage("What is the weather in Lyon? Use the tool.");
var answer = await chat.GetChatMessageContentAsync(history, settings, kernel);
Console.WriteLine("answer: " + answer.Content);

// Dispose flushes the exporter before the meta file is written; the using's own
// dispose at the end of main is then a no-op.
tracerProvider.Dispose();
Capture.WriteMeta();

public sealed class WeatherPlugin
{
    [KernelFunction("get_weather"), Description("Get the current weather for a city")]
    public string GetWeather([Description("City name")] string city) => $"Sunny in {city}, 21 C";
}
