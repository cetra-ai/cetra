// M8 spike, Microsoft Agent Framework .NET: one agent run (no tools), then one tool-call round.
// M8_MAF_MODE: "default" (no content switch), "code" (OpenTelemetryAgent.EnableSensitiveData = true),
// "env" (no code switch; OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true in the environment).
using System.ClientModel;
using System.ComponentModel;
using Microsoft.Agents.AI;
using Microsoft.Extensions.AI;
using OpenAI;
using OpenAI.Chat;
using OpenTelemetry;
using OpenTelemetry.Resources;
using OpenTelemetry.Trace;

var workdir = Environment.GetEnvironmentVariable("M7_OUT") ?? throw new InvalidOperationException("M7_OUT unset");
var baseUrl = File.ReadAllText(Path.Combine(workdir, "base.url")).Trim();
var mode = Environment.GetEnvironmentVariable("M8_MAF_MODE") ?? "default";
const string SourceName = "m8-maf";

using var tracerProvider = Sdk.CreateTracerProviderBuilder()
    .SetResourceBuilder(ResourceBuilder.CreateDefault().AddService("m8-maf"))
    .AddSource(SourceName)
    .AddSource(OpenTelemetryAgent.DefaultSourceName)
    .AddSource("Experimental.Microsoft.Extensions.AI")
    .AddOtlpExporter()
    .Build();

var openAi = new OpenAIClient(
    new ApiKeyCredential("sk-fake-m8"),
    new OpenAIClientOptions { Endpoint = new Uri(baseUrl + "/v1") });
ChatClient chatClient = openAi.GetChatClient("gpt-4o-mini");

Action<OpenTelemetryAgent> configure = mode == "code"
    ? o => o.EnableSensitiveData = true
    : _ => { };

// 1. one agent run, no tools
AIAgent plain = chatClient.AsAIAgent(name: "m8-agent", instructions: "Be brief.")
    .AsBuilder()
    .UseOpenTelemetry(sourceName: SourceName, configure: configure)
    .Build();
var chat = await plain.RunAsync("Say hello in one sentence.");
Console.WriteLine("CHAT: " + chat.Text);

// 2. one tool-call round: the model asks for read_file, the agent runs it, the model answers
[Description("Reads a file and returns its text.")]
static string ReadFile([Description("Absolute path of the file")] string path) => "hello from the M8 tool";

AIAgent withTool = chatClient.AsAIAgent(name: "m8-agent", instructions: "Be brief.",
        tools: [AIFunctionFactory.Create(ReadFile, "read_file")])
    .AsBuilder()
    .UseOpenTelemetry(sourceName: SourceName, configure: configure)
    .Build();
var round = await withTool.RunAsync("Read hello.txt and tell me what it says.");
Console.WriteLine("TOOL: " + round.Text);
