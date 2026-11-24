// Microsoft Agent Framework .NET 1.28.0 (IN8.4, from spikes/m8-files/dotnet/maf): one
// agent run with one tool round against the capture mock, traced by UseOpenTelemetry
// with EnableSensitiveData on. The endpoints and key come from the capture harness;
// the OTLP endpoint is set as the base URL because OpenTelemetry .NET 1.21.0 ignores
// OTEL_EXPORTER_OTLP_TRACES_ENDPOINT (spikes/m8-followup.md § 9.3).
using System.ClientModel;
using System.ComponentModel;
using Cetra.Capture;
using Microsoft.Agents.AI;
using Microsoft.Extensions.AI;
using OpenAI;
using OpenAI.Chat;
using OpenTelemetry;
using OpenTelemetry.Trace;

const string SourceName = "capture";

Environment.SetEnvironmentVariable("OTEL_EXPORTER_OTLP_ENDPOINT", Capture.OtlpBaseEndpoint());
using var tracerProvider = Sdk.CreateTracerProviderBuilder()
    .AddSource(SourceName)
    .AddSource(OpenTelemetryAgent.DefaultSourceName)
    .AddSource("Experimental.Microsoft.Extensions.AI")
    .AddOtlpExporter()
    .Build();

var openAi = new OpenAIClient(
    new ApiKeyCredential(Capture.MockKey()),
    new OpenAIClientOptions { Endpoint = new Uri(Capture.MockBaseUrl() + "/v1") });
ChatClient chatClient = openAi.GetChatClient("gpt-4o-mini");

[Description("Get the current weather for a city")]
static string GetWeather([Description("City name")] string city) => $"Sunny in {city}, 21 C";

AIAgent agent = chatClient.AsAIAgent(name: "capture-agent", instructions: "Be brief.",
        tools: [AIFunctionFactory.Create(GetWeather, "get_weather")])
    .AsBuilder()
    .UseOpenTelemetry(sourceName: SourceName, configure: o => o.EnableSensitiveData = true)
    .Build();
var answer = await agent.RunAsync("What is the weather in Lyon? Use the tool.");
Console.WriteLine("answer: " + answer.Text);

// Dispose flushes the exporter before the meta file is written; the using's own
// dispose at the end of main is then a no-op.
tracerProvider.Dispose();
Capture.WriteMeta();
