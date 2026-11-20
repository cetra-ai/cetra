# Prompt rendering cases

`render.json` holds the shared cases for the plain `{{ name }}` prompt syntax (D-V-3) and for chat prompts with placeholders (D-V-5). A case says what a template must scan to, what it must render to with the given values, or which error it must raise.

The `text` cases are run by `cetra-eval`'s `plain_template` tests in Rust and by the Python and TypeScript SDK tests. The `chat` cases are run by the SDK tests only, since the registry's chat rendering lives in the SDKs.

Positions in the `error` objects are 1-based lines and columns counted in Unicode scalar values. A JavaScript implementation must count code points, not UTF-16 code units.
