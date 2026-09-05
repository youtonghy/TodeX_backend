# Agent Instructions

- Treat requests that imply action (for example, “can you…”, “I want to…”, or “help me…”) as authorization to do the work. Infer routine details from the repository and conversation, persist until the requested outcome is complete, and do not stop at a plan or a capability acknowledgement.
- Before asking a question or requesting approval, complete all reversible, read-only, review, and implementation work already authorized by context so the user can review a concrete result. Ask only when the answer would materially change the outcome or when the next step is destructive or external.
- User instructions take precedence over general guidance in this file. If another instruction file or skill causes a pause or changes direction, identify that file and quote the relevant rule in the final response.
- Keep responses direct and technically precise. Lead with the result, use plain language and active voice, and use lists only for genuinely parallel or sequential information. Avoid filler, canned conclusions, unnecessary warnings, and unexplained jargon.
- For work that can be split independently, use collaboration tools to delegate parallel subtasks when doing so saves time or improves quality. Keep messages to other agents and user-facing text readable.
- Calibrate verification to risk: do not add tests that merely mirror a small reversible change; run the checks appropriate to the change and broaden them only when failures or unresolved risk justify it.
- When changing OpenAI API integrations, follow the current official model guidance (https://developers.openai.com/api/docs/guides/latest-model):
  - Use GPT-6 Astra (`gpt-6-astra`) for new general-purpose work unless compatibility, provider availability, latency, or cost requires another model. Keep model IDs configurable and verify the provider's supported-model list; do not silently fall back to an invented model name.
  - Prefer the Responses API for new integrations and for tool calling. Preserve conversation state and tool results using the Responses primitives rather than recreating Chat Completions message formats.
  - Set reasoning explicitly for the workload. Astra does not support `none`; use `low` or higher. Preserve an existing effective effort during migrations unless there is a documented reason to change it.
  - Do not send unsupported sampling or log-probability fields to Astra (`temperature`, `top_p`, `top_logprobs`, `logprobs`, or `message.output_text.logprobs`). Remove them when migrating from older Chat Completions integrations.
  - When migrating prompt caching, replace `prompt_cache_retention` with `prompt_cache_options.ttl: "30m"`, and review cache boundaries and cost implications.
  - Use Responses tool calling and validate structured tool inputs/outputs at the application boundary. Keep retries, timeouts, and streaming behavior compatible with the selected SDK and model.
  - Pin model versions for production paths when reproducibility matters, and record the selected model, reasoning effort, and fallback rationale in configuration or documentation. Re-run representative evaluations after model changes.
  - Prefer server-side state primitives, streaming, and background execution that match the workload; do not assume Chat Completions-only fields or response shapes when switching APIs.

- After completing each task, create one or more Git commits for the changes made in that task.
- Group commits by change category or repository responsibility when the task includes unrelated changes.
- Run the relevant validation commands before committing whenever practical, and mention any validation that could not be run.
- Push the created commits to the current branch's upstream remote after committing.
- If committing or pushing is blocked, report the blocker explicitly and leave the working tree status clear in the final response.
- Do not include unrelated local changes in a task commit. Preserve user changes unless the user explicitly asks to modify or discard them.

## OpenAI API and model guidance

- Prefer the Responses API for new integrations and tool-calling workflows; keep state, tools, structured outputs, streaming, and errors aligned with its current contract.
- Use GPT-6 Astra for complex or multi-step work when available. Select a smaller supported model only for clear cost or latency reasons, and keep the model configurable rather than hard-coded.
- Set `reasoning.effort` deliberately and verify that the selected provider/model supports the requested level. Use lower effort for simple, latency-sensitive requests and higher effort for complex planning, coding, research, or orchestration.
- During model migrations, verify provider model discovery and defaults, remove unsupported sampling or log-probability parameters, and recheck tool calls, schemas, token limits, streaming, and error handling.
- Review prompt caching after model changes: keep stable instructions and reusable context first, avoid dynamic prefixes, and confirm cache settings are supported.
- Pin and log the effective model and reasoning settings for reproducibility, without logging API keys or sensitive prompt/content data.
- Run representative integration checks for tool calls when practical and update configuration examples when defaults or model behavior change.
