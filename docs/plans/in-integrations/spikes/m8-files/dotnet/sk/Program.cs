// M8 spike, Semantic Kernel .NET: one chat call (no tools), then one tool-call round.
// M8_SK_SWITCH selects the AppContext switches: "none" (default), "on" (EnableOTelDiagnostics),
// "sensitive" (EnableOTelDiagnostics and EnableOTelDiagnosticsSensitive).
using System.ComponentModel;
using Microsoft.SemanticKernel;
using Microsoft.SemanticKernel.ChatCompletion;
using Microsoft.SemanticKernel.Connectors.OpenAI;
using OpenTelemetry;
using OpenTelemetry.Resources;
using OpenTelemetry.Trace;

var workdir = Environment.GetEnvironmentVariable("M7_OUT") ?? throw new InvalidOperationException("M7_OUT unset");
var baseUrl = File.ReadAllText(Path.Combine(workdir, "base.url")).Trim();
var mode = Environment.GetEnvironmentVariable("M8_SK_SWITCH") ?? "none";

if (mode is "on" or "sensitive")
    AppContext.SetSwitch("Microsoft.SemanticKernel.Experimental.GenAI.EnableOTelDiagnostics", true);
if (mode == "sensitive")
    AppContext.SetSwitch("Microsoft.SemanticKernel.Experimental.GenAI.EnableOTelDiagnosticsSensitive", true);

using var tracerProvider = Sdk.CreateTracerProviderBuilder()
    .SetResourceBuilder(ResourceBuilder.CreateDefault().AddService("m8-sk"))
    .AddSource("Microsoft.SemanticKernel*")
    .AddOtlpExporter()
    .Build();

var builder = Kernel.CreateBuilder();
builder.AddOpenAIChatCompletion(modelId: "gpt-4o-mini", apiKey: "sk-fake-m8", endpoint: new Uri(baseUrl + "/v1"));
var kernel = builder.Build();
kernel.ImportPluginFromType<FilePlugin>();
var chat = kernel.GetRequiredService<IChatCompletionService>();

// 1. one chat call, no function choice behavior, so no tools are sent
var first = new ChatHistory();
first.AddUserMessage("Say hello in one sentence.");
var chatReply = await chat.GetChatMessageContentAsync(first, kernel: kernel);
Console.WriteLine("CHAT: " + chatReply.Content);

// 2. one tool-call round: the model asks for read_file, SK runs it, the model answers
var settings = new OpenAIPromptExecutionSettings { FunctionChoiceBehavior = FunctionChoiceBehavior.Auto() };
var second = new ChatHistory();
second.AddUserMessage("Read hello.txt and tell me what it says.");
var toolReply = await chat.GetChatMessageContentAsync(second, settings, kernel);
Console.WriteLine("TOOL: " + toolReply.Content);

public sealed class FilePlugin
{
    [KernelFunction("read_file"), Description("Reads a file and returns its text.")]
    public string ReadFile([Description("Absolute path of the file")] string path) => "hello from the M8 tool";
}
