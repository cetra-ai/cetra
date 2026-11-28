package cetra.capture;

import java.io.IOException;
import java.io.InputStream;
import java.io.Writer;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Map;
import java.util.Properties;
import java.util.TreeMap;

/**
 * The Java capture harness: the endpoints and mock key the capture runner passes in the
 * environment, and the meta file it expects at the end.
 *
 * <p>A scenario program reads its URLs and key from here (never from its own code), makes
 * its model call, flushes and closes its tracer provider, then calls {@link #writeMeta()}
 * and exits 0.
 */
public final class Capture {

    private Capture() {}

    /** The mock provider's base URL, from {@code CETRA_CAPTURE_MOCK_URL}. It has no path. */
    public static String mockBaseUrl() {
        return required("CETRA_CAPTURE_MOCK_URL");
    }

    /** The receiver's full OTLP traces URL, from {@code CETRA_CAPTURE_OTLP_ENDPOINT}. */
    public static String otlpTracesEndpoint() {
        return required("CETRA_CAPTURE_OTLP_ENDPOINT");
    }

    /**
     * The receiver's base OTLP URL: the traces URL without its trailing {@code /v1/traces}.
     * Quarkus and .NET configure their exporter with this form.
     */
    public static String otlpBaseEndpoint() {
        String endpoint = otlpTracesEndpoint();
        String suffix = "/v1/traces";
        if (!endpoint.endsWith(suffix)) {
            throw new IllegalStateException(
                    "CETRA_CAPTURE_OTLP_ENDPOINT does not end in " + suffix);
        }
        return endpoint.substring(0, endpoint.length() - suffix.length());
    }

    /** The mock provider's API key, from {@code OPENAI_API_KEY}. */
    public static String mockKey() {
        return required("OPENAI_API_KEY");
    }

    private static String required(String name) {
        String value = System.getenv(name);
        if (value == null || value.isEmpty()) {
            throw new IllegalStateException(name + " is unset");
        }
        return value;
    }

    /**
     * Writes {@code CETRA_CAPTURE_META} with the versions of the libraries named in
     * {@code CETRA_CAPTURE_LIBRARIES}, read from {@code capture-versions.properties} on the
     * classpath (each scenario module filters its own copy from its pom's pins). Fails when a
     * listed name is missing, so a pin removed from the pom fails the capture, not the fixture.
     */
    public static void writeMeta() {
        String listed = required("CETRA_CAPTURE_LIBRARIES");
        Path metaPath = Path.of(required("CETRA_CAPTURE_META"));
        Properties versions = new Properties();
        // Quarkus's fast-jar loads classes through its own class loader, so the thread's
        // context loader is the one that sees every module's resources.
        InputStream stream = Thread.currentThread().getContextClassLoader()
                .getResourceAsStream("capture-versions.properties");
        if (stream == null) {
            throw new IllegalStateException("capture-versions.properties is not on the classpath");
        }
        try (stream) {
            versions.load(stream);
        } catch (IOException e) {
            throw new IllegalStateException("capture-versions.properties could not be read", e);
        }
        Map<String, String> libraries = new TreeMap<>();
        for (String name : listed.split(",")) {
            if (name.isEmpty()) {
                continue;
            }
            String version = versions.getProperty(name);
            if (version == null) {
                throw new IllegalStateException("capture-versions.properties is missing " + name);
            }
            libraries.put(name, version);
        }
        // Written by hand: the harness has no JSON dependency.
        StringBuilder json = new StringBuilder("{\"libraries\":{");
        boolean first = true;
        for (Map.Entry<String, String> entry : libraries.entrySet()) {
            if (!first) {
                json.append(',');
            }
            first = false;
            json.append('"')
                    .append(escape(entry.getKey()))
                    .append("\":\"")
                    .append(escape(entry.getValue()))
                    .append('"');
        }
        json.append("}}");
        try (Writer writer = Files.newBufferedWriter(metaPath, StandardCharsets.UTF_8)) {
            writer.write(json.toString());
        } catch (IOException e) {
            throw new IllegalStateException("the meta file could not be written", e);
        }
    }

    private static String escape(String text) {
        return text.replace("\\", "\\\\").replace("\"", "\\\"");
    }
}
