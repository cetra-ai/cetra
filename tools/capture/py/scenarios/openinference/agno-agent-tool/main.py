"""Entry point. The capture runner executes this file."""

import os

os.environ["AGNO_TELEMETRY"] = "false"

from openinference_agno_agent_tool import main

if __name__ == "__main__":
    main()
