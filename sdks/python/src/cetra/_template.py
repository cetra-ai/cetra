"""Plain ``{{ name }}`` prompt templates (D-V-3, D-V-5): the rules of ``fixtures/prompts``."""

import re
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from typing import Final

RESERVED: Final = frozenset(
    {
        "true",
        "false",
        "none",
        "True",
        "False",
        "None",
        "and",
        "or",
        "not",
        "in",
        "is",
        "if",
        "else",
        "loop",
        "self",
        "range",
        "dict",
        "namespace",
        "debug",
    }
)

_NAME: Final = re.compile(r"[A-Za-z_][A-Za-z0-9_]*", re.ASCII)


class PromptCompileError(ValueError):
    """A template is invalid, or a variable or placeholder has no value.

    ``kind`` is ``invalid``, ``missing_variable`` or ``missing_placeholder``. ``name`` is the
    missing name; ``reason``, ``line`` and ``column`` describe an invalid template. The message
    never quotes the template.
    """

    def __init__(
        self,
        kind: str,
        *,
        name: str | None = None,
        reason: str | None = None,
        line: int | None = None,
        column: int | None = None,
        prompt: str | None = None,
    ) -> None:
        self.kind = kind
        self.name = name
        self.reason = reason
        self.line = line
        self.column = column
        self.prompt = prompt
        if kind == "missing_variable":
            detail = f"missing variable {name}"
        elif kind == "missing_placeholder":
            detail = f"missing placeholder {name}"
        elif line is None or column is None:
            detail = f"invalid template: {reason}"
        else:
            detail = f"invalid template at line {line}, column {column}: {reason}"
        super().__init__(f"prompt {prompt}: {detail}" if prompt is not None else detail)


@dataclass(frozen=True, slots=True)
class _Text:
    text: str


@dataclass(frozen=True, slots=True)
class _Var:
    name: str


_Segment = _Text | _Var


def _invalid(line: int, column: int, reason: str) -> PromptCompileError:
    return PromptCompileError("invalid", reason=reason, line=line, column=column)


def _scan(source: str) -> list[_Segment]:
    """Splits ``source`` into text and variables; raises on the first invalid tag."""
    segments: list[_Segment] = []
    text_start = 0
    line = 1
    column = 1
    index = 0
    length = len(source)
    while index < length:
        ch = source[index]
        if ch == "{" and index + 1 < length:
            nxt = source[index + 1]
            if nxt in "%#":
                raise _invalid(line, column, "block")
            if nxt == "{":
                inner_start = index + 2
                end = source.find("}}", inner_start)
                if end < 0:
                    raise _invalid(line, column, "unclosed")
                name = source[inner_start:end].strip(" \t")
                if _NAME.fullmatch(name) is None:
                    raise _invalid(line, column, "not_a_name")
                if name in RESERVED:
                    raise _invalid(line, column, "reserved")
                if text_start < index:
                    segments.append(_Text(source[text_start:index]))
                segments.append(_Var(name))
                # A valid tag holds no newline, so it advances the column by its length.
                column += end + 2 - index
                index = end + 2
                text_start = index
                continue
        if ch == "\n":
            line += 1
            column = 1
        else:
            column += 1
        index += 1
    if text_start < length:
        segments.append(_Text(source[text_start:]))
    return segments


def variables(source: str) -> list[str]:
    """Variable names in order of first use. Raises PromptCompileError(kind="invalid")."""
    names: list[str] = []
    for segment in _scan(source):
        if isinstance(segment, _Var) and segment.name not in names:
            names.append(segment.name)
    return names


def render(source: str, values: Mapping[str, str]) -> str:
    """Substitutes every variable; raises PromptCompileError on an invalid template, then on
    the first missing variable in template order."""
    segments = _scan(source)
    for segment in segments:
        if isinstance(segment, _Var) and segment.name not in values:
            raise PromptCompileError("missing_variable", name=segment.name)
    parts: list[str] = []
    for segment in segments:
        if isinstance(segment, _Var):
            parts.append(values[segment.name])
        else:
            parts.append(segment.text)
    return "".join(parts)


@dataclass(frozen=True, slots=True)
class _Placeholder:
    name: str


@dataclass(frozen=True, slots=True)
class _Message:
    role: str
    content: str


_Entry = _Placeholder | _Message


def _checked_entries(template: Sequence[Mapping[str, object]]) -> list[_Entry]:
    """Validates every entry in order and raises on the first invalid one. A placeholder is
    ``type == "placeholder"`` with a string name, trimmed of spaces and tabs; a message needs
    string ``role`` and ``content``, and its content must scan."""
    checked: list[_Entry] = []
    for entry in template:
        if entry.get("type") == "placeholder":
            raw = entry.get("name")
            if not isinstance(raw, str):
                raise PromptCompileError("invalid", reason="not_a_message")
            name = raw.strip(" \t")
            if _NAME.fullmatch(name) is None:
                raise PromptCompileError("invalid", reason="not_a_name")
            checked.append(_Placeholder(name))
            continue
        role = entry.get("role")
        content = entry.get("content")
        if not isinstance(role, str) or not isinstance(content, str):
            raise PromptCompileError("invalid", reason="not_a_message")
        _scan(content)
        checked.append(_Message(role, content))
    return checked


def _names_of(checked: Sequence[_Entry]) -> tuple[list[str], list[str]]:
    names: list[str] = []
    placeholders: list[str] = []
    for entry in checked:
        if isinstance(entry, _Placeholder):
            if entry.name not in placeholders:
                placeholders.append(entry.name)
            continue
        for name in variables(entry.content):
            if name not in names:
                names.append(name)
    return names, placeholders


def chat_names(template: Sequence[Mapping[str, object]]) -> tuple[list[str], list[str]]:
    """(variables, placeholder names), each in order of first use across the messages."""
    return _names_of(_checked_entries(template))


def render_chat(
    template: Sequence[Mapping[str, object]],
    values: Mapping[str, str],
    placeholders: Mapping[str, Sequence[Mapping[str, str]]],
) -> list[dict[str, str]]:
    """Renders a chat template: messages are rendered, placeholders are replaced by copies of
    the given messages, unrendered. Raises PromptCompileError in the order of the rules in
    ``docs/plans/v-prompts/02-m2-sdk.md`` V2.1."""
    checked = _checked_entries(template)
    names, placeholder_names = _names_of(checked)
    for name in names:
        if name not in values:
            raise PromptCompileError("missing_variable", name=name)
    for name in placeholder_names:
        if name not in placeholders:
            raise PromptCompileError("missing_placeholder", name=name)
    rendered: list[dict[str, str]] = []
    for entry in checked:
        if isinstance(entry, _Placeholder):
            rendered.extend(dict(message) for message in placeholders[entry.name])
        else:
            rendered.append({"role": entry.role, "content": render(entry.content, values)})
    return rendered
