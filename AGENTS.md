# Agent Instructions

## Execution and communication

- Treat requests that imply action (for example, “can you…”, “I want to…”, or “help me…”) as authorization to do the work. Infer routine details from the repository and conversation, persist until the requested outcome is complete, and do not stop at a plan or a capability acknowledgement.
- Before asking a question or requesting approval, complete all reversible, read-only, review, and implementation work already authorized by context so the user can review a concrete result. Ask only when the answer would materially change the outcome or when the next step is destructive or external.
- User instructions take precedence over general guidance in this file. If an instruction file or skill blocks progress, link the exact file, quote the rule, and explain why it applies; distinguish an explicit requirement from your interpretation.
- Keep responses direct and technically precise. Lead with the result, use plain language and active voice, and use lists only for genuinely parallel or sequential information. Avoid filler, canned conclusions, unnecessary warnings, and unexplained jargon.
- For work that can be split independently, use collaboration tools to delegate parallel subtasks when doing so saves time or improves quality. Keep messages to other agents and user-facing text readable.
- Calibrate verification to risk: do not add tests that merely mirror a small reversible change; run the checks appropriate to the change and broaden them only when failures or unresolved risk justify it.
- Incorporate follow-up corrections into the active task and retain completed work. Answer side questions, then continue unless the user cancels or replaces the task.

## OpenAI API compatibility (GPT-6 Astra)

Reference: [Official model guidance](https://developers.openai.com/api/docs/guides/latest-model), checked 2026-09-06. Recheck before implementation; these rules apply to OpenAI integrations, not other providers or the coding agent's own runtime.

- Keep model IDs configurable; respect explicit selections and verify provider support. Consider `gpt-6-astra` for new work; preserve existing routing unless migration is requested.
- Astra tool calling requires Responses; text-only Chat Completions remains supported.
- Astra does not support `none`. On migration, map `none`/`minimal` effort to `low`; otherwise preserve effective effort and evaluate.
- Remove `temperature`, `top_p`, `top_logprobs`; also remove Chat Completions `logprobs` or Responses `include` entry `message.output_text.logprobs`.
- From GPT-5.5 or earlier, replace `prompt_cache_retention` with `prompt_cache_options.ttl: "30m"`; review billing.
- Async tools use `async: true` and original `call_id`; the application manages execution and pending work. Steering uses WebSockets.
- For compatible standard single-agent requests, change effort via `configuration_update`; retain request-level effort for caching.
- EU residency requires Standard processing, excluding `fast`/`priority`.

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
