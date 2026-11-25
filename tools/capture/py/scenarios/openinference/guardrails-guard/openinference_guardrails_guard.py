"""Guardrails guard with a local validator, traced through Guardrails' own spans."""

from __future__ import annotations

from typing import Any

from guardrails import Guard, OnFailAction
from guardrails.validators import (
    FailResult,
    PassResult,
    ValidationResult,
    Validator,
    register_validator,
)
from openinference.instrumentation.openai import OpenAIInstrumentor

from capture_py.harness import finish, mock_url, setup

MODEL = "gpt-4o-mini-2024-07-18"


@register_validator(name="no-banned-word", data_type="string")
class NoBannedWord(Validator):  # type: ignore[misc]
    """Reject text that contains the word secret."""

    def validate(
        self,
        value: Any,  # noqa: ANN401 - Validator.validate takes Any
        metadata: dict[str, Any],
    ) -> ValidationResult:
        if "secret" in value:
            return FailResult(error_message="contains banned word")
        return PassResult()


def main() -> None:
    """Guard one model call with a local validator."""
    provider = setup()
    OpenAIInstrumentor().instrument(tracer_provider=provider)
    guard = Guard(name="no-secrets").use(NoBannedWord(on_fail=OnFailAction.EXCEPTION))
    guard(
        model=MODEL,
        messages=[{"role": "user", "content": "What is the capital of France?"}],
        api_base=mock_url("/v1"),
        max_retries=0,
    )
    finish(provider)


if __name__ == "__main__":
    main()
