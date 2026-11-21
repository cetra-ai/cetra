# @cetra-ai/cli

`@cetra-ai/cli` (MIT) sets up [cetra](https://cetra.sh) tracing in an app with one command.
It signs you in, writes `.env` and `.cetra/project.json`, installs the cetra skill for your
coding agent, and prints the prompt to paste into that agent.

```sh
npx @cetra-ai/cli@latest init
```

- `cetra-cli init` signs in, picks a project, writes the keys and installs the skill.
- `cetra-cli query` runs a SQL query on the linked project.
- `cetra-cli login` signs in again and saves a new token.

Read the full guide at [cetra.sh/docs/get-started/coding-agent](https://cetra.sh/docs/get-started/coding-agent/).
