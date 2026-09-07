# Agent Instructions

Reference: [Official model guidance](https://developers.openai.com/api/docs/guides/latest-model), checked 2026-09-08.

## Goal and execution

- Complete the requested outcome within scope. Infer routine details from the repository and conversation; preserve unrelated behavior.
- Treat action requests (for example, “can you…”, “I want to…”, or “help me…”) as instructions to do the work. Continue through implementation, appropriate verification, and delivery; a plan or capability acknowledgement is not completion.
- Ask only when a missing decision materially changes the outcome and cannot be reasonably inferred, or an action needs authorization not already provided. Prepare the authorized work needed for a concrete review before requesting approval. Continue independent work while waiting; do not treat silence as approval.
- Carry forward prior authorization, constraints, and completed work. Apply follow-up corrections to the active task; answer side questions and then resume unless the user cancels or replaces it.

## Instruction handling

- Follow system and developer instructions first, then explicit user instructions, then applicable repository and skill guidance. Read the instruction files that govern the paths being changed; check relevant skills for conflicting or stale rules before relying on them.
- Do not turn a guideline or a routine implementation choice into an approval gate. If an instruction file or skill causes a pause or departure from the user's intent, link the exact file, quote the rule, and explain its applicability; distinguish the written requirement from your interpretation.
- When editing prompts or guidance, remove redundant or conflicting rules; define outcomes, constraints, and completion criteria instead of prescribing every step.

## Communication

- Use the user's language unless requested otherwise. Lead with the result or intended action, use plain language and active voice, and keep technical details relevant to the user's decision.
- Prefer concise paragraphs. Use lists for parallel or sequential information; avoid filler, canned conclusions, invented jargon, and contrastive phrases that introduce an unrequested alternative.
- Report meaningful progress during sustained work. Make the final response self-contained: changes, validation, and blockers; distinguish completed work, proposals, and unrun checks.

## Tools, delegation, and verification

- Batch independent searches and reads; sequence dependent operations and conflicting writes. Ground decisions in observed repository and tool results.
- Delegate bounded, independent subtasks when tools are available and parallel work improves speed or quality. Specify scope, file ownership, and expected evidence; review and integrate results. Handle small, tightly coupled edits locally. Keep inter-agent messages readable.
- Reuse repository patterns and check the diff for unintended changes. Select validation from the affected behavior and repository requirements; do not add tests that merely mirror a low-impact edit.
- Deliver once the requested outcome and required checks are satisfied. Repeat or broaden verification only for new changes, failures, or unresolved concerns.
- If blocked, try an available fallback within scope and finish independent work. If progress still needs user input or an external change, report the specific blocker and smallest next step.

## OpenAI API compatibility (GPT-6 Astra)

Recheck the relevant official documentation when changing an integration. These rules apply only to application requests to the OpenAI API; they do not configure the coding agent's runtime or establish support in other providers or gateways.

- Keep model IDs configurable; respect explicit selections and verify provider support. Consider `gpt-6-astra` for new work; preserve existing routing unless migration is requested.
- Astra tool calling requires Responses; text-only Chat Completions remains supported.
- For Astra API requests, use `reasoning.effort` in Responses or `reasoning_effort` in Chat Completions. Astra does not support `none`. On migration, map `none`/`minimal` effort to `low`; otherwise preserve effective effort and evaluate.
- For Astra requests only, remove `temperature`, `top_p`, `top_logprobs`; also remove Chat Completions `logprobs` or Responses `include` entry `message.output_text.logprobs`.
- When migrating from GPT-5.5 or earlier, replace `prompt_cache_retention` with `prompt_cache_options.ttl: "30m"`; review cache boundaries and cache-write billing.
- Async tools use `async: true` and original `call_id`; the application manages execution and pending work. Steering uses WebSockets.
- If effort must change between responses, use `configuration_update` in compatible standard single-agent requests; keep request-level `reasoning.effort` unchanged for caching. Verify compatibility before adopting this feature.
- For Astra with EU data residency, use Standard processing; `service_tier: "fast"` and `service_tier: "priority"` are unsupported.

## Integration verification

- Validate tool inputs and outputs at application boundaries. Check timeouts, retries, streaming, errors, and state preservation; avoid repeating non-idempotent actions during retries.
- Re-run representative evaluations after model or prompt changes. Record effective model and reasoning settings without logging secrets or sensitive content; update affected configuration examples.

## Git delivery

- After completing each task, create one or more Git commits for the changes made in that task.
- Group commits by change category or repository responsibility when the task includes unrelated changes.
- Run the relevant validation commands before committing whenever practical, and mention any validation that could not be run.
- Push the created commits to the current branch's upstream remote after committing.
- If committing or pushing is blocked, report the blocker explicitly and leave the working tree status clear in the final response.
- Do not include unrelated local changes in a task commit. Preserve user changes unless the user explicitly asks to modify or discard them.
