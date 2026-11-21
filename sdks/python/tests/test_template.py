import json
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any

import pytest

from cetra._template import PromptCompileError, chat_names, render, render_chat, variables

_CASES: dict[str, Any] = json.loads(
    (Path(__file__).parents[3] / "fixtures" / "prompts" / "render.json").read_text(encoding="utf-8")
)
_TEXT_CASES: list[Any] = _CASES["text"]
_CHAT_CASES: list[Any] = _CASES["chat"]


def _expected_error_fields(case: Mapping[str, Any]) -> dict[str, Any]:
    """The attributes the case's error must have, as a dict to compare with the exception."""
    error = case["error"]
    fields: dict[str, Any] = {"kind": error["kind"]}
    if error["kind"] == "missing_variable" or error["kind"] == "missing_placeholder":
        fields["name"] = error["name"]
    else:
        # A chat error may have no position (a reserved placeholder name).
        fields.update(reason=error["reason"], line=error.get("line"), column=error.get("column"))
    return fields


def _error_fields(err: PromptCompileError) -> dict[str, Any]:
    if err.kind == "invalid":
        return {"kind": err.kind, "reason": err.reason, "line": err.line, "column": err.column}
    return {"kind": err.kind, "name": err.name}


@pytest.mark.parametrize("case", _TEXT_CASES, ids=[case["name"] for case in _TEXT_CASES])
def test_shared_text_cases(case: Mapping[str, Any]) -> None:
    template: str = case["template"]
    values: dict[str, str] = case["values"]
    if case["variables"] is None:
        with pytest.raises(PromptCompileError) as info:
            variables(template)
        assert _error_fields(info.value) == _expected_error_fields(case)
    else:
        assert variables(template) == case["variables"]

    if "error" in case:
        with pytest.raises(PromptCompileError) as render_info:
            render(template, values)
        assert _error_fields(render_info.value) == _expected_error_fields(case)
    else:
        assert render(template, values) == case["expected"]


@pytest.mark.parametrize("case", _CHAT_CASES, ids=[case["name"] for case in _CHAT_CASES])
def test_shared_chat_cases(case: Mapping[str, Any]) -> None:
    template: Sequence[Mapping[str, object]] = case["template"]
    values: dict[str, str] = case["values"]
    placeholders: dict[str, Any] = case["placeholders"]
    if case["variables"] is None:
        # The names are not known: the template is refused while they are worked out.
        with pytest.raises(PromptCompileError) as names_info:
            chat_names(template)
        assert _error_fields(names_info.value) == _expected_error_fields(case)
    else:
        assert chat_names(template) == (case["variables"], case["placeholder_names"])

    if "error" in case:
        with pytest.raises(PromptCompileError) as info:
            render_chat(template, values, placeholders)
        assert _error_fields(info.value) == _expected_error_fields(case)
    else:
        assert render_chat(template, values, placeholders) == case["expected"]


def test_error_message_has_no_template_text() -> None:
    with pytest.raises(PromptCompileError) as info:
        render("{{ secret.x }}", {})
    assert "secret" not in str(info.value)

    with pytest.raises(PromptCompileError) as missing:
        render("{{secret_payload}}", {})
    assert missing.value.name == "secret_payload"
    assert "secret_payload" in str(missing.value)


def test_message_carries_the_prompt_name() -> None:
    err = PromptCompileError("missing_variable", name="a", prompt="greeting")
    assert str(err) == "prompt greeting: missing variable a"


def test_render_inserts_values_verbatim() -> None:
    assert render("{{a}}", {"a": "{{b}}"}) == "{{b}}"


def test_render_keeps_trailing_newline() -> None:
    assert render("{{a}}\n", {"a": "x"}) == "x\n"


def test_chat_message_without_role_is_refused() -> None:
    with pytest.raises(PromptCompileError) as info:
        render_chat([{"content": "x"}], {}, {})
    assert info.value.kind == "invalid"
    assert info.value.reason == "not_a_message"


def test_chat_placeholder_name_is_trimmed() -> None:
    template: list[Mapping[str, object]] = [{"type": "placeholder", "name": " h\t"}]
    assert chat_names(template) == ([], ["h"])
    assert render_chat(template, {}, {"h": [{"role": "user", "content": "x"}]}) == [
        {"role": "user", "content": "x"}
    ]


def test_chat_reserved_placeholder_name_is_refused() -> None:
    template: list[Mapping[str, object]] = [{"type": "placeholder", "name": "loop"}]
    with pytest.raises(PromptCompileError) as info:
        chat_names(template)
    assert info.value.reason == "reserved"
    assert info.value.line is None
    assert str(info.value) == "invalid template: reserved"


def test_chat_copies_placeholder_messages() -> None:
    given: dict[str, str] = {"role": "user", "content": "{{raw}}"}
    out = render_chat([{"type": "placeholder", "name": "h"}], {}, {"h": [given]})
    assert out == [given]
    assert out[0] is not given
