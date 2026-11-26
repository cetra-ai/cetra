"""The MCP server of the scenario: one get_weather tool over stdio, traced by the harness."""

from mcp.server.mcpserver import MCPServer

from capture_py.harness import setup

server = MCPServer("weather")


@server.tool()
def get_weather(city: str) -> str:
    """Look up the weather for a city."""
    return f'{{"temperature_c": 18, "conditions": "sunny", "city": "{city}"}}'


def main() -> None:
    """Export the server's spans as they end, and serve until the client closes stdin."""
    setup()
    server.run("stdio")


if __name__ == "__main__":
    main()
