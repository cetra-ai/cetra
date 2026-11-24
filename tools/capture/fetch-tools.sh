# Fetch the pinned gateway binaries that capture scenarios run (IN5.1) into
# target/capture-tools/. Linux x86_64 only; every download is checked by SHA-256.
# The binary is moved to its final name last, so a partial run never leaves a file
# that looks complete.
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
tools_dir="$repo_root/target/capture-tools"

temp_paths=()
cleanup() {
    if [ "${#temp_paths[@]}" -gt 0 ]; then
        rm -rf -- "${temp_paths[@]}"
    fi
}
trap cleanup EXIT

if [ "$(uname -s)" != "Linux" ] || [ "$(uname -m)" != "x86_64" ]; then
    echo "capture-tools: the pinned binaries are not available on this platform (Linux x86_64 only)"
    exit 0
fi

# fetch <final-path> <url> <sha256-of-the-download> [<archive-member>]
fetch() {
    local final="$1" url="$2" sha="$3" member="${4:-}"
    if [ -x "$final" ]; then
        echo "capture-tools: $final already present"
        return 0
    fi
    local dir
    dir="$(dirname -- "$final")"
    mkdir -p -- "$dir"
    local download="$dir/download.tmp"
    temp_paths+=("$download")
    echo "capture-tools: fetching $(basename -- "$final")"
    curl -fsSL --output "$download" "$url"
    echo "$sha  $download" | sha256sum -c -
    local binary="$download"
    if [ -n "$member" ]; then
        local extract_dir="$dir/extract.tmp"
        rm -rf -- "$extract_dir"
        mkdir -p -- "$extract_dir"
        temp_paths+=("$extract_dir")
        tar -xzf "$download" -C "$extract_dir" "$member"
        binary="$extract_dir/$member"
    fi
    chmod +x -- "$binary"
    mv -f -- "$binary" "$final"
    echo "capture-tools: $(basename -- "$final") in place"
}

fetch "$tools_dir/temporal/1.10.1/temporal" \
    "https://github.com/temporalio/cli/releases/download/v1.10.1/temporal_cli_1.10.1_linux_amd64.tar.gz" \
    "4c1500e2ce2d4de136e5fe8852d9b7f82eb62f19b6b383b3e0c040519a9d20b1" \
    "temporal"
fetch "$tools_dir/bifrost/2.5.1/bifrost-http" \
    "https://downloads.getmaxim.ai/bifrost/v2.5.1/linux/amd64/bifrost-http" \
    "2b629808bdd54dca5abb38f0e62f6250e7fbd8c336c45d568d83d683974c0734"
fetch "$tools_dir/aigw/1.2.1/aigw" \
    "https://github.com/envoyproxy/ai-gateway/releases/download/v1.2.1/aigw-linux-amd64" \
    "b97af14fc3f116ccbdce7cdb7ae98da999f41623285bbf253095f09d214523d7"
fetch "$tools_dir/envoy/1.39.1/envoy" \
    "https://github.com/envoyproxy/envoy/releases/download/v1.39.1/envoy-1.39.1-linux-x86_64" \
    "002c6e1c69ed0fa0ea381887247cadadfaec9481375fa8d8d2b1731eeabf40b8"
