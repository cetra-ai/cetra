"""The site's Python API page names every public name and nothing else (D-W-8)."""

import re
from pathlib import Path

import pytest

import cetra

PAGE = (
    Path(__file__).resolve().parents[3] / "website/src/content/docs/docs/reference/python/api.mdx"
)


@pytest.mark.skipif(not PAGE.exists(), reason="the docs site is not in this tree")
def test_the_api_page_covers_all() -> None:
    headings = re.findall(r"^### `([A-Za-z_][A-Za-z0-9_]*)`$", PAGE.read_text("utf-8"), re.M)
    assert len(headings) == len(set(headings)), "a name is documented twice"
    assert set(headings) == set(cetra.__all__)
