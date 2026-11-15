"""The retry delay shared by the SDK's HTTP senders (spec 10 § Score delivery, plan J4.1)."""

from typing import Final

import requests

RETRY_BACKOFF_S: Final = (1.0, 2.0, 4.0)
RETRY_AFTER_CAP_S: Final = 30.0


def retry_delay(response: requests.Response | None, attempt: int) -> float:
    """``Retry-After`` seconds when a non-negative integer (capped at 30 s), else 1/2/4 s."""
    if response is not None:
        header = response.headers.get("Retry-After")
        if header is not None:
            # Spec 10 (Score delivery): ASCII digits, surrounding whitespace ignored. "+5",
            # "1_0" and fullwidth digits are not integers here, so they use the default.
            value = header.strip()
            if value.isascii() and value.isdigit():
                try:
                    seconds = int(value)
                except ValueError:  # more digits than int() converts: far above the cap
                    return RETRY_AFTER_CAP_S
                # Cap before float(): a very long digit string must not overflow the conversion.
                return float(min(seconds, RETRY_AFTER_CAP_S))
    return RETRY_BACKOFF_S[attempt]
