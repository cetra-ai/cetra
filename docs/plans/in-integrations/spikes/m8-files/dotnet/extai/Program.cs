// M8 spike, Microsoft.Extensions.AI: one chat call (no tools), then one tool-call round.
// The fake provider (tools/server.py) answers the first request that carries tools with a tool call,
// and every later request with text. Base URL comes from $M7_OUT/base.url (written by inner.sh).
using System.ComponentModel;
using System.ClientModel;
using Microsoft.Extensions.AI;
using OpenAI;
using OpenTelemetry;
using OpenTelemetry.Resources;
using OpenTelemetry.Trace;

var workdir = Environment.GetEnvironmentVariable("M7_OUT") ?? throw new InvalidOperationException("M7_OUT unset");
var baseUrl = File.ReadAllText(Path.Combine(workdir, "base.url")).Trim();
var sensitive = Environment.GetEnvironmentVariable("M8_SENSITIVE") == "1";

using var tracerProvider = Sdk.CreateTracerProviderBuilder()
    .SetResourceBuilder(ResourceBuilder.CreateDefault().AddService("m8-extai"))
    .AddSource("Experimental.Microsoft.Extensions.AI")
    .AddOtlpExporter()
    .Build();

var openAi = new OpenAIClient(
    new ApiKeyCredential("sk-fake-m8"),
    new OpenAIClientOptions { Endpoint = new Uri(baseUrl + "/v1") });
IChatClient inner = openAi.GetChatClient("gpt-4o-mini").AsIChatClient();

IChatClient client = new ChatClientBuilder(inner)
    .UseFunctionInvocation()
    .UseOpenTelemetry(sourceName: "Experimental.Microsoft.Extensions.AI",
        configure: c => c.EnableSensitiveData = sensitive)
    .Build();

// 1. one chat call, no tools offered
var chat = await client.GetResponseAsync("Say hello in one sentence.");
Console.WriteLine("CHAT: " + chat.Text);

// 2. one tool-call round: the model asks for read_file, the framework runs it, the model answers
[Description("Reads a file and returns its text.")]
static string ReadFile([Description("Absolute path of the file")] string path) => "hello from the M8 tool";

var tool = AIFunctionFactory.Create(ReadFile, "read_file");
var round = await client.GetResponseAsync("Read hello.txt and tell me what it says.",
    new ChatOptions { Tools = [tool] });
Console.WriteLine("TOOL: " + round.Text);
