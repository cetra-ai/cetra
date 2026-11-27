# Build the Java and .NET capture harnesses and scenarios (IN8.1), with the
# JDK, Maven and .NET SDK that fetch-tools.sh fetched into target/capture-tools/.
# Called online from fetch-tools.sh's last line, and only on Linux x86_64 (the
# platform check is upstream). Each build is skipped when its inputs are
# unchanged since the last one: a sha256 of the input files (never anything
# under target/, bin/ or obj/) is stored next to the output.
#   Java:   target/capture-tools/java-build.sha256 covers every pom, source and
#           properties file under tools/capture/java; the Maven build also writes
#           target/classpath.txt per module (the capture-classpath execution),
#           from which this script writes each module's target/capture.args
#           argument file (-cp for plain modules, -jar for Quarkus fast-jars).
#   .NET:   <project>/bin/capture/.inputs.sha256 covers the project's sources
#           and lock file plus the Harness sources; the build runs restore
#           --locked-mode and build into bin/capture, then shuts the build
#           server down (a server left running keeps the parent waiting).
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
# The Java argument file (target/capture.args) is whitespace-split by the JVM, so a
# classpath entry with a space in it cannot be carried: refuse such a checkout rather
# than build output the runner cannot run.
case "$repo_root" in
    *[[:space:]]*)
        echo "capture-tools: the repo path contains whitespace, which the Java argument file cannot carry: $repo_root" >&2
        exit 1
        ;;
esac
tools_dir="$repo_root/target/capture-tools"
jdk="$tools_dir/jdk/21.0.13+9"
maven="$tools_dir/maven/3.9.16"
m2="$tools_dir/m2"
dotnet_sdk="$tools_dir/dotnet/10.0.404"
nuget="$tools_dir/nuget"
java_root="$repo_root/tools/capture/java"
dotnet_root="$repo_root/tools/capture/dotnet"

temp_paths=()
cleanup() {
    if [ "${#temp_paths[@]}" -gt 0 ]; then
        rm -rf -- "${temp_paths[@]}"
    fi
}
trap cleanup EXIT

java_modules=()
if [ -f "$java_root/pom.xml" ]; then
    # Every file under tools/capture/java, never anything under target/, as one
    # comparable hash list (paths relative to the repo root). The JDK and Maven
    # versions and the Maven repository's absolute path are part of the inputs:
    # capture.args embeds the classpath from that repository, so a moved checkout
    # or a toolchain bump must rebuild.
    java_inputs="$(
        cd -- "$repo_root"
        printf 'm2 %s\n' "$tools_dir/m2"
        printf 'jdk %s\n' "$(basename -- "$jdk")"
        printf 'maven %s\n' "$(basename -- "$maven")"
        find tools/capture/java -type d -name target -prune -o \
            -type f -print0 | LC_ALL=C sort -z | xargs -0 -r sha256sum
    )"
    for pom in "$java_root"/scenarios/*/*/pom.xml; do
        [ -f "$pom" ] || continue
        java_modules+=("$(dirname -- "$pom")")
    done

    java_hash="$tools_dir/java-build.sha256"
    java_up_to_date=false
    if [ -f "$java_hash" ] && [ "$java_inputs" = "$(cat -- "$java_hash")" ]; then
        java_up_to_date=true
        for module in "${java_modules[@]}"; do
            # A stale build can leave capture.args behind without the file it is
            # written from (or the Quarkus jar it runs), so both are required.
            if [ ! -f "$module/target/capture.args" ] || {
                [ ! -f "$module/target/quarkus-app/quarkus-run.jar" ] &&
                    [ ! -f "$module/target/classpath.txt" ]
            }; then
                java_up_to_date=false
                break
            fi
        done
    fi

    if [ "$java_up_to_date" = true ]; then
        echo "capture-tools: ${#java_modules[@]} Java scenarios up to date"
    else
        for module in "${java_modules[@]}"; do
            rm -f -- "$module/target/capture.args"
        done
        JAVA_HOME="$jdk" "$maven/bin/mvn" -B -q -Dmaven.repo.local="$m2" package -DskipTests \
            -f "$java_root/pom.xml"
        for module in "${java_modules[@]}"; do
            if [ -f "$module/target/quarkus-app/quarkus-run.jar" ]; then
                printf '%s\n' '-jar target/quarkus-app/quarkus-run.jar' \
                    > "$module/target/capture.args"
            else
                classpath="$(cat -- "$module/target/classpath.txt")"
                printf '%s\n' "-cp target/classes:$classpath capture.Main" \
                    > "$module/target/capture.args"
            fi
        done
        printf '%s\n' "$java_inputs" > "$java_hash"
        echo "capture-tools: built ${#java_modules[@]} Java scenarios"
    fi
fi

dotnet_total=0
dotnet_built=0
for csproj in "$dotnet_root"/scenarios/*/*/Capture.csproj; do
    [ -f "$csproj" ] || continue
    dotnet_total=$((dotnet_total + 1))
    project="$(dirname -- "$csproj")"
    marker="$project/bin/capture/.inputs.sha256"
    # Every file of the project and of the Harness, never anything under bin/ or
    # obj/ (so scenario.json counts too), plus the SDK version: a toolchain bump
    # must rebuild.
    inputs="$(
        cd -- "$repo_root"
        printf 'dotnet %s\n' "$(basename -- "$dotnet_sdk")"
        {
            find "${project#"$repo_root"/}" -type d \( -name bin -o -name obj \) -prune -o \
                -type f -print0
            find tools/capture/dotnet/Harness -type d \( -name bin -o -name obj \) -prune -o \
                -type f -print0
        } | LC_ALL=C sort -z | xargs -0 -r sha256sum
    )"
    if [ -f "$marker" ] && [ "$inputs" = "$(cat -- "$marker")" ]; then
        continue
    fi
    dotnet_built=$((dotnet_built + 1))
    cli_home="$(mktemp -d)"
    temp_paths+=("$cli_home")
    dotnet_env=(env DOTNET_ROOT="$dotnet_sdk" DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_NOLOGO=1
        DOTNET_CLI_HOME="$cli_home" NUGET_PACKAGES="$nuget")
    "${dotnet_env[@]}" "$dotnet_sdk/dotnet" restore --locked-mode -p:NuGetAudit=false "$csproj"
    "${dotnet_env[@]}" "$dotnet_sdk/dotnet" build --no-restore -p:NuGetAudit=false \
        -p:UseSharedCompilation=false -nodeReuse:false -o "$project/bin/capture" "$csproj"
    "${dotnet_env[@]}" "$dotnet_sdk/dotnet" build-server shutdown
    printf '%s\n' "$inputs" > "$marker"
done
if [ "$dotnet_built" -eq 0 ]; then
    echo "capture-tools: $dotnet_total .NET scenarios up to date"
else
    echo "capture-tools: built $dotnet_built .NET scenarios"
fi
