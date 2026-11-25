// Microsoft.Extensions.AI 11.0.0 (IN8.4, from spikes/m8-files/dotnet/extai): one chat
// call with one tool round against the capture mock, traced by UseOpenTelemetry with
// EnableSensitiveData on. The endpoints and key come from the capture harness; the OTLP
// endpoint is set as the base URL because OpenTelemetry .NET 1.21.0 ignores
// OTEL_EXPORTER_OTLP_TRACES_ENDPOINT (spikes/m8-followup.md § 9.3).
using System.ClientModel;
using System.ComponentModel;
using Cetra.Capture;
using Microsoft.Extensions.AI;
using OpenAI;
using OpenTelemetry;
using OpenTelemetry.Trace;

Environment.SetEnvironmentVariable("OTEL_EXPORTER_OTLP_ENDPOINT", Capture.OtlpBaseEndpoint());
using var tracerProvider = Sdk.CreateTracerProviderBuilder()
    .AddSource("Experimental.Microsoft.Extensions.AI")
    .AddOtlpExporter()
    .Build();

var openAi = new OpenAIClient(
    new ApiKeyCredential(Capture.MockKey()),
    new OpenAIClientOptions { Endpoint = new Uri(Capture.MockBaseUrl() + "/v1") });
IChatClient inner = openAi.GetChatClient("gpt-4o-mini").AsIChatClient();
IChatClient client = new ChatClientBuilder(inner)
    .UseFunctionInvocation()
    .UseOpenTelemetry(sourceName: "Experimental.Microsoft.Extensions.AI",
        configure: c => c.EnableSensitiveData = true)
    .Build();

[Description("Get the current weather for a city")]
static string GetWeather([Description("City name")] string city) => $"Sunny in {city}, 21 C";

var tool = AIFunctionFactory.Create(GetWeather, "get_weather");
var answer = await client.GetResponseAsync("What is the weather in Lyon? Use the tool.",
    new ChatOptions { Tools = [tool] });
Console.WriteLine("answer: " + answer.Text);

// Dispose flushes the exporter before the meta file is written; the using's own
// dispose at the end of main is then a no-op.
tracerProvider.Dispose();
Capture.WriteMeta();
