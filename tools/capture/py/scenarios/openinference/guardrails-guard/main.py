"""Entry point. The capture runner executes this file."""

import os
import tempfile
from pathlib import Path

os.environ["LITELLM_LOCAL_MODEL_COST_MAP"] = "True"
# Guardrails reads ~/.guardrailsrc at import time; a fresh HOME turns its usage
# metrics off for this run. Setting them on the settings object is too late.
home = Path(tempfile.mkdtemp(prefix="cetra-guardrails-"))
(home / ".guardrailsrc").write_text("enable_metrics=false\n", encoding="utf-8")
os.environ["HOME"] = str(home)

from openinference_guardrails_guard import main  # noqa: E402

if __name__ == "__main__":
    main()
