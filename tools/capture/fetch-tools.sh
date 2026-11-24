# Fetch the pinned gateway binaries that capture scenarios run (IN5.1) into
# target/capture-tools/, and install the LiteLLM Proxy environment (Amendment B),
# the Langflow tool, the n8n and Promptfoo tools and the Python environments under
# tools/capture/envs/ (IN6.1).
# Linux x86_64 only; every download is checked by SHA-256.
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
    local download
    download="$(mktemp --tmpdir="$dir")"
    temp_paths+=("$download")
    echo "capture-tools: fetching $(basename -- "$final")"
    curl -fsSL --retry 3 --connect-timeout 30 --output "$download" "$url"
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
    # `mktemp` creates the download with mode 0600, so the mode is set in full.
    chmod 755 -- "$binary"
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
fetch "$tools_dir/opencode-v2/2.3.0/opencode" \
    "https://github.com/anomalyco/opencode/releases/download/v2.3.0/opencode-linux-x64.tar.gz" \
    "fc0f81522aceeabac47693d88389efa2bc517ce74c2c38fd6967819fcc6e4d3b" \
    "opencode"
fetch "$tools_dir/goose/1.59.2/goose" \
    "https://github.com/aaif-goose/goose/releases/download/v1.59.2/goose-x86_64-unknown-linux-gnu.tar.gz" \
    "c63cf82953e968d84b7542eb38a621dfdb5050b967728ed363517be5cdc666ea" \
    "./goose"

# The LiteLLM Proxy environment: its own uv project (litellm[proxy] cannot share
# tools/capture/py; its mcp and rich pins conflict with the frameworks there),
# installed as a pinned tool. Reinstalled only when the committed lock changes,
# which a copy stored next to the environment detects.
litellm_env="$tools_dir/litellm-proxy/1.109.1"
litellm_lock_copy="$tools_dir/litellm-proxy/uv.lock"
if [ -x "$litellm_env/bin/litellm" ] &&
    cmp -s "$repo_root/tools/capture/litellm-proxy/uv.lock" "$litellm_lock_copy"; then
    echo "capture-tools: litellm-proxy 1.109.1 already present"
else
    echo "capture-tools: installing litellm-proxy 1.109.1"
    UV_PROJECT_ENVIRONMENT="$litellm_env" uv sync --frozen --project "$repo_root/tools/capture/litellm-proxy"
    mkdir -p -- "$(dirname -- "$litellm_lock_copy")"
    cp -- "$repo_root/tools/capture/litellm-proxy/uv.lock" "$litellm_lock_copy"
fi

# Installs a pnpm tool project (tools/capture/<name>) into target/capture-tools/<name>/<version>/node_modules.
# Skipped when the marker file exists and the committed lock matches its copy.
# pnpm_tool <name> <version> <marker-under-node_modules>
pnpm_tool() {
    local name="$1" version="$2" marker="$3"
    local project="$repo_root/tools/capture/$name"
    local modules="$tools_dir/$name/$version/node_modules"
    local lock_copy="$tools_dir/$name/pnpm-lock.yaml"
    if [ -e "$modules/$marker" ] && cmp -s "$project/pnpm-lock.yaml" "$lock_copy"; then
        echo "capture-tools: $name $version already present"
        return 0
    fi
    echo "capture-tools: installing $name $version"
    pnpm install --frozen-lockfile --dir "$project" --modules-dir "$modules"
    mkdir -p -- "$tools_dir/$name"
    cp -- "$project/pnpm-lock.yaml" "$lock_copy"
}

# Langflow, the same way as the LiteLLM Proxy above: a pinned uv tool project with no code.
langflow_env="$tools_dir/langflow/1.13.2"
langflow_lock_copy="$tools_dir/langflow/uv.lock"
if [ -x "$langflow_env/bin/langflow" ] &&
    cmp -s "$repo_root/tools/capture/langflow/uv.lock" "$langflow_lock_copy"; then
    echo "capture-tools: langflow 1.13.2 already present"
else
    echo "capture-tools: installing langflow 1.13.2"
    UV_PROJECT_ENVIRONMENT="$langflow_env" uv sync --frozen --project "$repo_root/tools/capture/langflow"
    mkdir -p -- "$(dirname -- "$langflow_lock_copy")"
    cp -- "$repo_root/tools/capture/langflow/uv.lock" "$langflow_lock_copy"
fi

# Hermes Agent (IN7.9): its setup.py refuses to build a wheel outside a Nix build, so it cannot
# be a dependency of a uv project of ours. It is installed the way the spike ran it: cloned at
# the tag and synced from its own committed uv.lock (which pins its transitive versions, not our
# 7-day rule), with the hermes-otel plugin installed by commit into that environment.
hermes_env="$tools_dir/hermes/0.23.2"
hermes_src="$tools_dir/hermes/src-0.23.2"
if [ -e "$hermes_env/.complete" ]; then
    echo "capture-tools: hermes 0.23.2 already present"
else
    echo "capture-tools: installing hermes 0.23.2"
    # A failed attempt leaves no .complete marker, so nothing of it is kept.
    rm -rf -- "$hermes_src" "$hermes_env"
    mkdir -p -- "$tools_dir/hermes"
    git clone --depth 1 --branch v0.23.2 https://github.com/NousResearch/hermes-agent "$hermes_src"
    hermes_commit="$(git -C "$hermes_src" rev-parse HEAD)"
    if [ "$hermes_commit" != "ae2a81ccc00e9b7c06301523e5bb09608397149c" ]; then
        echo "capture-tools: hermes v0.23.2 is $hermes_commit, expected ae2a81ccc00e9b7c06301523e5bb09608397149c" >&2
        exit 1
    fi
    UV_PROJECT_ENVIRONMENT="$hermes_env" uv sync --frozen --project "$hermes_src"
    uv pip install --python "$hermes_env/bin/python" "hermes-otel @ git+https://github.com/briancaffey/hermes-otel@32554cf76c8dbeb8d382eaff9007112abf3f7b2c"
    touch "$hermes_env/.complete"
fi

pnpm_tool n8n 2.48.4 n8n/bin/n8n
pnpm_tool promptfoo 0.125.1 promptfoo/package.json
pnpm_tool openclaw 2026.11.5 openclaw/package.json
pnpm_tool opencode 1.19.6 opencode-ai/package.json
pnpm_tool oh-my-pi 18.22.0 @oh-my-pi/pi-coding-agent/package.json

# The OpenClaw diagnostics exporter (IN7.0), installed into a staged home so the capture never
# writes to the operator's ~/.openclaw. Skipped when the staged home already lists the plugin.
openclaw_dir="$tools_dir/openclaw/2026.11.5"
if [ -d "$openclaw_dir/home/.openclaw" ] && grep -rq diagnostics-otel "$openclaw_dir/home/.openclaw"; then
    echo "capture-tools: openclaw diagnostics-otel 2026.11.5 already present"
else
    echo "capture-tools: installing openclaw diagnostics-otel 2026.11.5"
    HOME="$openclaw_dir/home" XDG_CONFIG_HOME="$openclaw_dir/home/.config" \
        XDG_DATA_HOME="$openclaw_dir/home/.local/share" XDG_STATE_HOME="$openclaw_dir/home/.local/state" \
        XDG_CACHE_HOME="$openclaw_dir/home/.cache" \
        "$openclaw_dir/node_modules/.bin/openclaw" plugins install --pin --accept-capabilities npm:@openclaw/diagnostics-otel@2026.11.5
fi

# The Python environments under tools/capture/envs/ (D-IN-26): each is synced in place
# (its .venv), and skipped when its committed lock matches the copy stored in tools/capture-tools.
mkdir -p -- "$tools_dir/envs"
for env_dir in "$repo_root"/tools/capture/envs/*/; do
    env_name="$(basename -- "$env_dir")"
    env_lock_copy="$tools_dir/envs/$env_name.lock"
    if [ -d "$env_dir/.venv" ] && cmp -s "$env_dir/uv.lock" "$env_lock_copy"; then
        echo "capture-tools: envs/$env_name already present"
        continue
    fi
    echo "capture-tools: syncing envs/$env_name"
    uv sync --frozen --project "$env_dir"
    cp -- "$env_dir/uv.lock" "$env_lock_copy"
done

# Chromium for Browser Use (IN6.1): the browser-use environment's pinned playwright installs it into
# target/capture-tools/playwright (the binary is pinned by that lock, not checksummed). Installed
# only when a chromium-*/INSTALLATION_COMPLETE marker exists and a copy of the browser-use uv.lock,
# kept beside it, matches the committed lock; a mismatch reinstalls.
playwright_dir="$tools_dir/playwright"
browser_use_lock="$repo_root/tools/capture/envs/browser-use/uv.lock"
browser_use_lock_copy="$playwright_dir/uv.lock"
if compgen -G "$playwright_dir/chromium-*/INSTALLATION_COMPLETE" >/dev/null &&
    cmp -s "$browser_use_lock" "$browser_use_lock_copy"; then
    echo "capture-tools: chromium already present"
else
    echo "capture-tools: installing chromium for browser-use"
    PLAYWRIGHT_BROWSERS_PATH="$playwright_dir" uv run --project "$repo_root/tools/capture/envs/browser-use" --frozen playwright install chromium
    mkdir -p -- "$playwright_dir"
    cp -- "$browser_use_lock" "$browser_use_lock_copy"
fi
