"""Entry point. The capture runner executes this file."""

import os

os.environ["HAYSTACK_TELEMETRY_ENABLED"] = "False"

from openinference_haystack_pipeline import main

if __name__ == "__main__":
    main()
