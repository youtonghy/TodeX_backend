# Agent Instructions

Reference: [Official model guidance — prompting best practices](https://developers.openai.com/api/docs/guides/latest-model#prompting-best-practices), checked 2026-09-10. The workflow below adapts that guidance to TodeX; it does not select the coding agent's model or change runtime permissions.

## Goal and execution

- Complete the requested outcome within scope. Infer routine details from the repository and conversation; preserve unrelated behavior. Define success by the requested behavior, constraints, and verification evidence.
- Treat action requests (for example, “can you…”, “I want to…”, or “help me…”) as instructions to do the work. Continue through implementation, appropriate verification, and delivery; a plan or capability acknowledgement is not completion.
- Proceed with routine, reversible work already authorized by the task. Ask only for missing decisions that materially change the outcome and cannot be inferred, or for authorization not already provided. Prepare a concrete, reviewable result before requesting final approval; continue independent work while waiting. Silence is not approval.
- Carry forward prior authorization, constraints, and completed work. Apply follow-up corrections to the active task; answer side questions and then resume unless the user cancels or replaces it.

## Instruction handling

- Follow system and developer instructions first, then explicit user instructions, then applicable repository and skill guidance. Read instructions governing the changed paths and load relevant skills; resolve stale or conflicting guidance against that priority.
- Do not turn routine choices or hypothetical risks into approval gates. If a file or skill causes a pause or departure from the user's intent, link the exact file, quote the rule, and explain its applicability. Distinguish the requirement from your interpretation.
- When editing prompts or guidance, remove redundant or conflicting rules. Define outcomes, constraints, evidence, and completion criteria; prescribe a sequence only when order matters.

## Communication

- Use the user's language unless requested otherwise. Lead with the result or intended action; use plain language, active voice, and technical detail appropriate to the reader.
- Prefer concise paragraphs. Use lists for parallel or sequential information; avoid filler, canned conclusions, invented jargon, and contrastive phrases that introduce an unrequested alternative.
- Start tool-based work with a brief update. During sustained work, report meaningful findings and next steps. Make the final response self-contained: changes, validation, and blockers; distinguish completed work, proposals, and unrun checks.

## Tools, delegation, and verification

- Use `rg` for repository searches. Batch independent searches and reads; sequence dependent operations and conflicting writes. Ground decisions in observed repository and tool results.
- Delegate bounded, independent subtasks when supported by the active harness and parallel work improves speed or quality. Specify scope, file ownership, and expected evidence; review and integrate results. Handle small, tightly coupled edits locally. Keep inter-agent messages readable.
- Reuse repository patterns and inspect the final diff for unintended changes. Select checks from the affected behavior and [CI requirements](.github/workflows/checks.yml). For documentation-only edits, check content, links, and `git diff --check`; run code checks when the change affects executable behavior.
- Add tests for meaningful behavior or regression risks. Avoid tests that merely mirror a low-impact edit. Once required checks pass, broaden or repeat them only for new changes, failures, or unresolved concerns; then deliver.
- If blocked, try an available fallback within scope and finish independent work. Report the specific blocker, checks not run, and smallest next step. Never present an attempted tool call or a planned check as successful execution.

## Provider and model compatibility

- TodeX uses native provider protocols, including Codex app-server. Follow the affected adapter and [API contract](docs/API.md); OpenAI HTTP API parameters do not establish support in CLI protocols, other providers, or gateways.
- Keep model IDs configurable and respect explicit selections. Use provider capability catalogs for models and supported reasoning efforts. Preserve routing, defaults, and protocol-specific parameter names unless the task calls for a change.
- For direct OpenAI API integration or migration work, recheck the [current migration requirements](https://developers.openai.com/api/docs/guides/latest-model#update-api-and-model-parameters) for the selected model: endpoints, tools, reasoning, unsupported parameters, caching, and service tiers. Adopt async tools or steering only when the application supports their execution and state lifecycle.
- Validate tool inputs and outputs at application boundaries. Check relevant timeouts, retries, streaming, errors, and state preservation; avoid repeating non-idempotent actions during retries.
- Evaluate representative tasks after model or application-prompt changes. Record effective model and reasoning settings without logging secrets or sensitive content; update affected configuration examples.

## Git delivery

- After completing each task, create one or more Git commits for the changes made in that task.
- Group commits by change category or repository responsibility when the task includes unrelated changes.
- Run the relevant validation commands before committing whenever practical, and mention any validation that could not be run.
- Push the created commits to the current branch's upstream remote after committing.
- If committing or pushing is blocked, report the blocker explicitly and leave the working tree status clear in the final response.
- Do not include unrelated local changes in a task commit. Preserve user changes unless the user explicitly asks to modify or discard them.
