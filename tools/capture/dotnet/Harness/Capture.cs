using System;
using System.Collections.Generic;
using System.IO;
using System.Text.Json;

namespace Cetra.Capture;

/// <summary>
/// The .NET capture harness: the endpoints and mock key the capture runner passes in the
/// environment, and the meta file it expects at the end.
/// </summary>
/// <remarks>
/// A scenario program reads its URLs and key from here (never from its own code), makes its
/// model call, disposes its tracer provider, then calls <see cref="WriteMeta"/> and exits 0.
/// </remarks>
public static class Capture
{
    /// <summary>The mock provider's base URL, from CETRA_CAPTURE_MOCK_URL. It has no path.</summary>
    public static string MockBaseUrl() => Required("CETRA_CAPTURE_MOCK_URL");

    /// <summary>The receiver's full OTLP traces URL, from CETRA_CAPTURE_OTLP_ENDPOINT.</summary>
    public static string OtlpTracesEndpoint() => Required("CETRA_CAPTURE_OTLP_ENDPOINT");

    /// <summary>
    /// The receiver's base OTLP URL: the traces URL without its trailing "/v1/traces".
    /// The .NET exporter is configured with this form.
    /// </summary>
    public static string OtlpBaseEndpoint()
    {
        const string suffix = "/v1/traces";
        var endpoint = OtlpTracesEndpoint();
        if (!endpoint.EndsWith(suffix, StringComparison.Ordinal))
        {
            throw new InvalidOperationException(
                "CETRA_CAPTURE_OTLP_ENDPOINT does not end in " + suffix);
        }
        return endpoint[..^suffix.Length];
    }

    /// <summary>The mock provider's API key, from OPENAI_API_KEY.</summary>
    public static string MockKey() => Required("OPENAI_API_KEY");

    private static string Required(string name) =>
        Environment.GetEnvironmentVariable(name) is { Length: > 0 } value
            ? value
            : throw new InvalidOperationException($"{name} is unset");

    /// <summary>
    /// Writes CETRA_CAPTURE_META with the versions of the libraries named in
    /// CETRA_CAPTURE_LIBRARIES, read from packages.lock.json in the working directory
    /// (each scenario project commits its own). Fails when a listed name is missing, so a
    /// pin removed from the project fails the capture, not the fixture.
    /// </summary>
    public static void WriteMeta()
    {
        var listed = Required("CETRA_CAPTURE_LIBRARIES").Split(',', StringSplitOptions.RemoveEmptyEntries);
        var metaPath = Required("CETRA_CAPTURE_META");
        using var locks = JsonDocument.Parse(File.ReadAllText("packages.lock.json"));
        if (!locks.RootElement.TryGetProperty("dependencies", out var dependencies)
            || !dependencies.TryGetProperty("net10.0", out var resolved))
        {
            throw new InvalidOperationException("packages.lock.json has no net10.0 dependencies");
        }
        var libraries = new SortedDictionary<string, string>();
        foreach (var name in listed)
        {
            if (!resolved.TryGetProperty(name, out var package))
            {
                throw new InvalidOperationException($"packages.lock.json is missing {name}");
            }
            var version = package.GetProperty("resolved").GetString();
            if (version is null)
            {
                throw new InvalidOperationException(
                    $"packages.lock.json library {name} has no resolved version");
            }
            libraries[name] = version;
        }
        var meta = new MemoryStream();
        using (var writer = new Utf8JsonWriter(meta))
        {
            writer.WriteStartObject();
            writer.WriteStartObject("libraries");
            foreach (var (name, version) in libraries)
            {
                writer.WriteString(name, version);
            }
            writer.WriteEndObject();
            writer.WriteEndObject();
        }
        File.WriteAllBytes(metaPath, meta.ToArray());
    }
}
