"""Entry point. The capture runner executes this file."""

import os

os.environ["LITELLM_LOCAL_MODEL_COST_MAP"] = "True"

from openinference_beeai_agent import main

if __name__ == "__main__":
    main()
