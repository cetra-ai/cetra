import importlib.metadata
import tomllib
from pathlib import Path

import cetra


def test_version_matches_the_project() -> None:
    pyproject = Path(__file__).parents[1] / "pyproject.toml"
    with pyproject.open("rb") as project_file:
        project = tomllib.load(project_file)
    version = project["project"]["version"]
    assert cetra.__version__ == version
    assert cetra.__version__ == importlib.metadata.version("cetra-sdk")
